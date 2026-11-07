//! The job queue of a project (TER-71): a real `terrarium_host::Client`
//! runs the jobs on this machine, against a bare git repo as the remote.
//! The recipe is a stand-in (`test -f OK`), as the lead asked: the real
//! one (`just ci::validate`) comes with TER-69.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::TestDaemon;
use serde_json::{Value, json};
use terrarium_core::protocol::HostCapabilities;
use terrarium_host::{Client, ClientOptions};

/// How long a poll waits before the test fails. A bound for a hung test
/// only: each poll returns as soon as its state is there.
const DEADLINE: Duration = Duration::from_secs(90);

#[tokio::test]
async fn host_caps_accept_names_ids_and_ssh_aliases() {
    let td = TestDaemon::start().await;
    let h = td
        .post("/api/hosts", json!({"name": "machine"}))
        .await
        .ok()
        .clone();
    let id = h["id"].as_str().unwrap();
    // Legacy rows were stored under names, and aliases may have distinct caps.
    let legacy_id = id.to_owned();
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .write("operator", move |t| {
            t.execute(
                "INSERT INTO job_hosts VALUES ('machine', 1, 'operator', 'old')",
                [],
            )?;
            t.execute(
                "INSERT INTO job_hosts VALUES (?1, 3, 'operator', 'new')",
                [&legacy_id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let legacy = td.get("/api/job-hosts").await.ok().clone();
    assert_eq!(legacy["hosts"].as_array().unwrap().len(), 1);
    assert_eq!(legacy["hosts"][0]["host"], id);
    assert_eq!(legacy["hosts"][0]["heavy_jobs"], 1);
    let cap = td
        .put("/api/job-hosts/machine", json!({"heavy_jobs": 2}))
        .await
        .ok()
        .clone();
    assert_eq!(cap["hosts"][0]["host"], id);
    let cap = td
        .put(&format!("/api/job-hosts/{id}"), json!({"heavy_jobs": 1}))
        .await
        .ok()
        .clone();
    assert_eq!(cap["hosts"].as_array().unwrap().len(), 1);
    assert_eq!(cap["hosts"][0]["heavy_jobs"], 1);
    let rows: i64 = td
        .daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .read(|c| Ok(c.query_row("SELECT count(*) FROM job_hosts", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(rows, 1, "a cap write must remove legacy aliases");
    td.delete(&format!("/api/job-hosts/{id}")).await.ok();
    let cap = td
        .put("/api/job-hosts/localhost", json!({"heavy_jobs": 2}))
        .await
        .ok()
        .clone();
    let key = cap["hosts"][0]["host"].as_str().unwrap();
    assert!(key.parse::<std::net::IpAddr>().is_ok());
    let cap = td
        .put(&format!("/api/job-hosts/{key}"), json!({"heavy_jobs": 1}))
        .await
        .ok()
        .clone();
    assert_eq!(cap["hosts"].as_array().unwrap().len(), 1);
    assert_eq!(cap["hosts"][0]["host"], key);
    assert_eq!(cap["hosts"][0]["heavy_jobs"], 1);
}

#[tokio::test]
async fn host_counts_respect_the_readers_project() {
    use terrariumd::store::jobs::{self, JobKind, NewJob};
    let td = TestDaemon::start().await;
    for project in ["p", "q"] {
        td.project(project).await;
        let card = td
            .post(
                &format!("/api/projects/{project}/cards"),
                json!({"title": "job"}),
            )
            .await
            .ok()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let project = project.to_owned();
        td.daemon
            .as_ref()
            .unwrap()
            .state
            .db
            .write("operator", move |t| {
                let j = jobs::create(
                    t,
                    NewJob {
                        project,
                        card,
                        kind: JobKind::Validate,
                        batch: Vec::new(),
                        repo: "app".into(),
                        branch: "next".into(),
                        commit: None,
                        by_agent: None,
                    },
                )?;
                jobs::start(t, &j.id, "machine")?;
                jobs::set_host_cap(t, "machine", Some(2))?;
                Ok(())
            })
            .await
            .unwrap();
    }
    for (name, roles, all) in [
        ("worker", json!([]), false),
        ("lead", json!(["project_admin"]), false),
        ("architect", json!(["architect"]), true),
        ("sysadmin", json!(["sysadmin"]), true),
    ] {
        td.agent("p", name, json!({"roles": roles})).await;
        let v = td
            .get_as(&format!("p/{name}"), "/api/job-hosts")
            .await
            .ok()
            .clone();
        let h = &v["hosts"][0];
        assert_eq!(h["running"], 2);
        assert_eq!(h["projects"]["p"], 1);
        assert_eq!(h["projects"].get("q").is_some(), all);
        assert_eq!(h.get("updated_by").is_some(), all);
        assert_eq!(h.get("updated_at").is_some(), all);
    }
    let v = td.get("/api/job-hosts").await.ok().clone();
    assert_eq!(v["hosts"][0]["projects"], json!({"p": 1, "q": 1}));
    assert!(v["hosts"][0].get("updated_by").is_some());
    td.project("empty").await;
    td.agent("empty", "worker", json!({})).await;
    let v = td
        .get_as("empty/worker", "/api/job-hosts")
        .await
        .ok()
        .clone();
    assert_eq!(v["hosts"][0]["projects"], json!({"empty": 0}));
}

/// TER-105: an SSH target on an address of a host agent's machine counts
/// on the host agent: `job-hosts` shows one host, and its cap holds across
/// the host-agent target and the SSH target.
#[tokio::test]
async fn an_ssh_alias_of_a_host_agent_is_one_host() {
    use terrariumd::store::jobs::{self, JobKind, NewJob};
    let td = TestDaemon::start().await;
    let id = td.post("/api/hosts", json!({"name": "helios"})).await.ok()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    td.post("/api/hosts", json!({"name": "other"})).await.ok();
    for (project, target) in [
        (
            "p",
            json!({"name": "p-build", "kind": "host", "host": "helios", "project": "p"}),
        ),
        (
            "q",
            json!({"name": "q-vm", "kind": "ssh", "address": "ci@10.3.0.2:22", "project": "q"}),
        ),
    ] {
        td.project(project).await;
        td.post("/api/targets", target.clone()).await.ok();
        td.put(
            &format!("/api/projects/{project}/delivery"),
            json!({"target": target["name"], "recipe": "true"}),
        )
        .await
        .ok();
    }
    let db = td.daemon.as_ref().unwrap().state.db.clone();
    // A job of q runs on the SSH target, stored under its address. No job
    // is queued, so the daemon's own tick starts nothing during the test.
    let card = td
        .post("/api/projects/q/cards", json!({"title": "job"}))
        .await
        .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    db.write("operator", move |t| {
        let j = jobs::create(
            t,
            NewJob {
                project: "q".into(),
                card,
                kind: JobKind::Validate,
                batch: Vec::new(),
                repo: "app".into(),
                branch: "next".into(),
                commit: None,
                by_agent: None,
            },
        )?;
        jobs::start(t, &j.id, "10.3.0.2")?;
        Ok(())
    })
    .await
    .unwrap();
    td.put("/api/job-hosts/helios", json!({"heavy_jobs": 1}))
        .await
        .ok();
    // The host of the queue of p, its running jobs, and its cap: what the
    // pick checks before a job of p starts.
    let room = |db: terrariumd::db::Db, project: &'static str| async move {
        db.read(move |c| jobs::host_room(c, project)).await.unwrap()
    };
    // Without an alias: two hosts, and helios has room for a job of p.
    let v = td.get("/api/job-hosts").await.ok().clone();
    assert_eq!(v["hosts"].as_array().unwrap().len(), 2, "{v}");
    assert_eq!(room(db.clone(), "p").await, Some((id.clone(), 0, 1)));
    // 10.3.0.2 has no cap of its own.
    assert_eq!(room(db.clone(), "q").await, None);

    // Only a sysadmin or the operator sets aliases.
    td.agent("q", "worker", json!({})).await;
    td.put_as(
        "q/worker",
        "/api/job-hosts/helios/aliases",
        json!({"aliases": ["10.3.0.2"]}),
    )
    .await
    .err(403, "forbidden");
    td.put(
        "/api/job-hosts/nope/aliases",
        json!({"aliases": ["10.3.0.2"]}),
    )
    .await
    .err(404, "not_found");
    td.put(
        "/api/job-hosts/helios/aliases",
        json!({"aliases": ["other"]}),
    )
    .await
    .err(422, "invalid");

    let v = td
        .put(
            "/api/job-hosts/helios/aliases",
            json!({"aliases": ["ci@10.3.0.2:22", "Helios.LAN"]}),
        )
        .await
        .ok()
        .clone();
    let hosts = v["hosts"].as_array().unwrap();
    assert_eq!(hosts.len(), 1, "{v}");
    assert_eq!(hosts[0]["host"], id);
    assert_eq!(hosts[0]["name"], "helios");
    assert_eq!(hosts[0]["aliases"], json!(["10.3.0.2", "helios.lan"]));
    assert_eq!(hosts[0]["running"], 1);
    assert_eq!(hosts[0]["projects"]["q"], 1);
    assert_eq!(hosts[0]["heavy_jobs"], 1);
    // The cap holds across both targets: the q job on the SSH target
    // fills helios, so the p job waits.
    assert_eq!(room(db.clone(), "p").await, Some((id.clone(), 1, 1)));
    assert_eq!(room(db.clone(), "q").await, Some((id.clone(), 1, 1)));
    // A worker sees the name, not the aliases.
    let v = td.get_as("q/worker", "/api/job-hosts").await.ok().clone();
    assert_eq!(v["hosts"][0]["name"], "helios");
    assert!(v["hosts"][0].get("aliases").is_none());

    // One host agent per alias.
    td.put(
        "/api/job-hosts/other/aliases",
        json!({"aliases": ["10.3.0.2"]}),
    )
    .await
    .err(409, "conflict");
    // A cap set by the alias lands on the host agent.
    let v = td
        .put("/api/job-hosts/10.3.0.2", json!({"heavy_jobs": 2}))
        .await
        .ok()
        .clone();
    assert_eq!(v["hosts"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v["hosts"][0]["host"], id);
    assert_eq!(v["hosts"][0]["heavy_jobs"], 2);
    assert_eq!(room(db.clone(), "p").await, Some((id.clone(), 1, 2)));
    td.put("/api/job-hosts/helios", json!({"heavy_jobs": 1}))
        .await
        .ok();

    // An empty list takes the aliases away, and so does a revoke.
    td.put("/api/job-hosts/helios/aliases", json!({"aliases": []}))
        .await
        .ok();
    assert_eq!(room(db.clone(), "p").await, Some((id.clone(), 0, 1)));
    td.put(
        "/api/job-hosts/helios/aliases",
        json!({"aliases": ["10.3.0.2"]}),
    )
    .await
    .ok();
    assert_eq!(room(db.clone(), "p").await, Some((id.clone(), 1, 1)));
    td.delete("/api/hosts/helios").await.ok();
    let left: i64 = db
        .read(|c| Ok(c.query_row("SELECT count(*) FROM job_host_aliases", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(left, 0);
}

struct Lab {
    td: TestDaemon,
    _dirs: tempfile::TempDir,
    /// The bare repo: the remote.
    origin: PathBuf,
    /// A clone to make branches in.
    work: PathBuf,
    /// The project admin `p/lead`.
    lead: String,
    /// The worker `p/w1`.
    w1: String,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A daemon with host agent `lab1`, the project target `p-box`, project
/// `p` with repo `app` (a bare repo with `main` and `next`), the lead
/// and worker `w1`, and the queue set: `test -f OK`, `slots`.
async fn lab(slots: u32) -> Lab {
    lab_with(slots, None).await
}

/// [`lab`] with a `docker` binary for the host agent.
async fn lab_with(slots: u32, docker: Option<&Path>) -> Lab {
    let td = TestDaemon::start().await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    let (_task, addr) = terrariumd::hosts::serve(state, "127.0.0.1:0")
        .await
        .unwrap();
    let r = td.post("/api/hosts", json!({"name": "lab1"})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let token = r.body["token"].as_str().unwrap().to_owned();
    let mut o = ClientOptions::new(format!("ws://{addr}/hosts/connect"), &token, "lab1");
    o.capabilities = HostCapabilities {
        shell: Some("/bin/sh".into()),
        ..Default::default()
    };
    if let Some(d) = docker {
        o.capabilities.docker = terrarium_core::protocol::ToolCapability {
            present: true,
            version: Some("29.8.0".into()),
        };
        o.docker_bin = d.display().to_string();
    }
    o.backoff_min = Duration::from_millis(50);
    o.backoff_max = Duration::from_millis(200);
    let dirs = tempfile::tempdir().unwrap();
    o.paths = terrarium_host::HostPaths {
        work_dir: td.state.with_file_name("work"),
        instances_dir: dirs.path().join("instances"),
        repos_dir: dirs.path().join("repos"),
    };
    tokio::spawn(Client::new(o).run());
    let start = Instant::now();
    while td.get("/api/hosts/lab1").await.body["online"] != true {
        assert!(start.elapsed() < DEADLINE, "lab1 never came online");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    td.project("p").await;
    td.project("q").await;
    // The columns of the delivery process, as on the terrarium board.
    let r = td
        .patch(
            "/api/projects/p/board",
            json!({"columns": [
                {"name": "backlog"}, {"name": "ready"},
                {"name": "in_progress", "role": "working"}, {"name": "validating"},
                {"name": "review"}, {"name": "accepted"},
                {"name": "blocked", "role": "blocked"}, {"name": "done"}]}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let r = td
        .post(
            "/api/targets",
            json!({"name": "p-box", "kind": "host", "host": "lab1", "project": "p"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let r = td
        .post(
            "/api/targets",
            json!({"name": "q-box", "kind": "host", "host": "lab1", "project": "q"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let r = td
        .post(
            "/api/targets",
            json!({"name": "gbox", "kind": "host", "host": "lab1"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);

    let origin = dirs.path().join("origin.git");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "--bare", "-b", "main"]);
    let work = dirs.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("README"), "one\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "one"]);
    git(
        &work,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&work, &["push", "-q", "origin", "main", "main:next"]);
    let r = td
        .post(
            "/api/projects/p/repos",
            json!({"url": origin.display().to_string(), "name": "app"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);

    let lead = td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let w1 = td.agent("p", "w1", json!({})).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let work_dir = dirs.path().join("jobs");
    let r = td
        .put(
            "/api/projects/p/delivery",
            json!({"target": "p-box", "recipe": "test -f OK", "slots": slots,
                   "next_repos": ["app"], "work_dir": work_dir.display().to_string()}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    Lab {
        td,
        _dirs: dirs,
        origin,
        work,
        lead,
        w1,
    }
}

impl Lab {
    /// Make branch `name` from `base` with `files`, push it, and return
    /// its head.
    fn branch(&self, name: &str, base: &str, files: &[(&str, &str)]) -> String {
        git(&self.work, &["fetch", "-q", "origin"]);
        git(
            &self.work,
            &["checkout", "-q", "-B", name, &format!("origin/{base}")],
        );
        for (f, text) in files {
            std::fs::write(self.work.join(f), text).unwrap();
        }
        git(&self.work, &["add", "-A"]);
        git(&self.work, &["commit", "-q", "-m", name]);
        git(&self.work, &["push", "-q", "-f", "origin", name]);
        git(&self.work, &["rev-parse", "HEAD"])
    }

    /// A card in `backlog`, assigned to `w1`, on `branch`.
    async fn card(&self, title: &str, branch: &str) -> String {
        let r = self
            .td
            .post(
                "/api/projects/p/cards",
                json!({"title": title, "body": "the card body", "branch": branch,
                       "column": "backlog", "assignee": self.w1}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
        r.body["id"].as_str().unwrap().to_owned()
    }

    async fn card_of(&self, id: &str) -> Value {
        self.td.get(&format!("/api/cards/{id}")).await.ok().clone()
    }

    /// Poll the card until `f` holds.
    async fn until_card(&self, id: &str, what: &str, f: impl Fn(&Value) -> bool) -> Value {
        let start = Instant::now();
        loop {
            let c = self.card_of(id).await;
            if f(&c) {
                return c;
            }
            assert!(
                start.elapsed() < DEADLINE,
                "card {id} never got {what}; last {c}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn jobs(&self) -> Vec<Value> {
        self.td.get("/api/projects/p/jobs").await.ok()["jobs"]
            .as_array()
            .unwrap()
            .clone()
    }

    async fn until_idle(&self) -> Vec<Value> {
        let start = Instant::now();
        loop {
            let v = self.td.get("/api/projects/p/jobs").await.ok().clone();
            if v["running"] == 0 && v["queued"] == 0 {
                return v["jobs"].as_array().unwrap().clone();
            }
            assert!(start.elapsed() < DEADLINE, "the queue never drained: {v}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn labels(c: &Value) -> Vec<String> {
    c["labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap().to_owned())
        .collect()
}

fn comments(c: &Value) -> String {
    c["comments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["body"].as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn the_config_is_checked_and_audited() {
    let lab = lab(2).await;
    let td = &lab.td;
    let v = td.get("/api/projects/p/delivery").await;
    assert_eq!(v.ok()["delivery"]["recipe"], "test -f OK");
    assert_eq!(v.ok()["enabled"], true);
    assert_eq!(v.ok()["slots"], 2);
    // Another project's target: no.
    td.put(
        "/api/projects/p/delivery",
        json!({"target": "q-box", "recipe": "true"}),
    )
    .await
    .err(403, "forbidden");
    // A global target: the operator yes, a project admin no.
    td.put_as(
        &lab.lead,
        "/api/projects/p/delivery",
        json!({"target": "gbox", "recipe": "true"}),
    )
    .await
    .err(403, "forbidden");
    // A worker may not change it.
    td.put_as(
        &lab.w1,
        "/api/projects/p/delivery",
        json!({"target": "p-box", "recipe": "true"}),
    )
    .await
    .err(403, "forbidden");
    // Bad values.
    td.put(
        "/api/projects/p/delivery",
        json!({"target": "p-box", "slots": 0}),
    )
    .await
    .err(422, "invalid");
    td.put(
        "/api/projects/p/delivery",
        json!({"target": "p-box", "next_repos": ["nope"]}),
    )
    .await
    .err(422, "invalid");
    // The lead changes the recipe: the audit keeps the old and the new.
    let r = td
        .put_as(
            &lab.lead,
            "/api/projects/p/delivery",
            json!({"target": "p-box", "recipe": "true", "next_repos": ["app"]}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let audit = td
        .get("/api/audit?action=project.delivery&order=desc")
        .await;
    let last = &audit.ok()[0]["detail"];
    assert_eq!(last["old"]["recipe"], "test -f OK", "{last}");
    assert_eq!(last["new"]["recipe"], "true", "{last}");
    // No recipe: the queue is off and a validate is refused.
    td.put("/api/projects/p/delivery", json!({"target": "p-box"}))
        .await
        .ok();
    let id = lab.card("c", "ter-x").await;
    let r = td
        .post_as(&lab.w1, &format!("/api/cards/{id}/validate"), json!({}))
        .await;
    r.err(409, "conflict");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("delivery set"),
        "{}",
        r.body
    );
}

#[tokio::test]
async fn a_green_validation_moves_the_card_to_review() {
    let lab = lab(2).await;
    let head = lab.branch("ter-a", "next", &[("OK", "yes\n")]);
    let id = lab.card("green", "ter-a").await;
    // A commit that is not the head of the pushed branch is refused.
    let r = lab
        .td
        .post_as(
            &lab.w1,
            &format!("/api/cards/{id}/validate"),
            json!({"commit": "0123456789abcdef0123456789abcdef01234567"}),
        )
        .await;
    r.err(409, "conflict");
    // Another worker may not queue it.
    let w2 = lab.td.agent("p", "w2", json!({})).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    lab.td
        .post_as(
            &w2,
            &format!("/api/cards/{id}/validate"),
            json!({"commit": head}),
        )
        .await
        .err(403, "forbidden");
    let r = lab
        .td
        .post_as(
            &lab.w1,
            &format!("/api/cards/{id}/validate"),
            json!({"commit": &head[..12], "comment": "done by w1"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["job"]["kind"], "validate");
    assert_eq!(r.body["queue"]["position"], 1, "{}", r.body);
    assert_eq!(r.body["queue"]["slots"], 2);
    // At once: validating, still w1's, by:w1 and a job label.
    let c = lab.card_of(&id).await;
    assert_eq!(c["column"], "validating");
    assert_eq!(c["assignee"], lab.w1.as_str(), "{c}");
    let l = labels(&c);
    assert!(l.contains(&"by:w1".to_owned()), "{l:?}");
    assert!(l.iter().any(|l| l.starts_with("validation:")), "{l:?}");
    let c = lab
        .until_card(&id, "review", |c| c["column"] == "review")
        .await;
    let l = labels(&c);
    assert!(!l.iter().any(|l| l.starts_with("validation:")), "{l:?}");
    assert!(
        c["assignee"].is_null(),
        "the final move frees the card: {c}"
    );
    let text = comments(&c);
    assert!(text.contains("passed"), "{text}");
    assert!(text.contains("done by w1"), "{text}");
    let jobs = lab.until_idle().await;
    let j = jobs.iter().find(|j| j["state"] == "passed").unwrap();
    assert_eq!(j["head"].as_str().unwrap(), head);
    assert_eq!(j["exit_code"], 0);
}

/// TER-165: the daemon finds the columns by role. `validating` is renamed
/// and keeps its role. `review` has no role and `qa` has it, so the role
/// wins over the name. `accepted` has no role, so its name is the fallback.
#[tokio::test]
async fn the_job_queue_finds_its_columns_by_role_then_by_name() {
    let lab = lab(2).await;
    let r = lab
        .td
        .patch(
            "/api/projects/p/board",
            json!({"columns": [
                {"name": "backlog"}, {"name": "ready"},
                {"name": "in_progress", "role": "working"},
                {"name": "checking", "rename_from": "validating", "role": "validating"},
                // A column that omits `role` keeps its old role: the default
                // board gives `review` the role `review`, so say null.
                {"name": "review", "role": null}, {"name": "qa", "role": "review"},
                {"name": "accepted", "role": null},
                {"name": "blocked", "role": "blocked"}, {"name": "done"}]}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    // A validate pass: the card is in the role column, then in `qa`.
    let head = lab.branch("ter-a", "next", &[("OK", "yes\n")]);
    let a = lab.card("by role", "ter-a").await;
    let r = lab
        .td
        .post_as(
            &lab.w1,
            &format!("/api/cards/{a}/validate"),
            json!({"commit": head}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let c = lab.card_of(&a).await;
    assert_eq!(c["column"], "checking", "{c}");
    lab.until_card(&a, "qa", |c| c["column"] == "qa").await;
    // A merge pass: the name `accepted` is the fallback.
    let head = lab.branch("ter-c", "next", &[("OK", "yes\n")]);
    let b = lab.card("by name", "ter-c").await;
    let r = lab
        .td
        .post_as(&lab.lead, &format!("/api/cards/{b}/merge"), json!({}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["job"]["kind"], "merge");
    lab.until_card(&b, "accepted", |c| c["column"] == "accepted")
        .await;
    let next = git(&lab.origin, &["rev-parse", "next"]);
    assert_ne!(next, head);
}

/// TER-107: a validate leaves one `validation:*` label and one `by:` label,
/// even when the card already has stale ones.
#[tokio::test]
async fn validate_replaces_stale_validation_and_by_labels() {
    let lab = lab(2).await;
    let head = lab.branch("ter-labels", "next", &[("OK", "yes\n")]);
    let id = lab.card("stale labels", "ter-labels").await;
    // Stale labels: another worker's `by:` and two `validation:` labels.
    lab.td
        .patch(
            &format!("/api/cards/{id}"),
            json!({"add_labels": ["by:w9", "validation:running", "validation:queued"]}),
        )
        .await
        .ok();
    // The seed is on the card before the validate, or the test proves nothing.
    let seeded = lab.card_of(&id).await;
    let l = labels(&seeded);
    assert!(l.contains(&"by:w9".to_owned()), "{seeded}");
    assert!(l.contains(&"validation:running".to_owned()), "{seeded}");
    let r = lab
        .td
        .post_as(
            &lab.w1,
            &format!("/api/cards/{id}/validate"),
            json!({"commit": head}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let by_of = |c: &Value| -> Vec<String> {
        labels(c)
            .into_iter()
            .filter(|l| l.starts_with("by:"))
            .collect()
    };
    let validation_of = |c: &Value| -> Vec<String> {
        labels(c)
            .into_iter()
            .filter(|l| l.starts_with("validation:"))
            .collect()
    };
    // At once: still w1's, one by:w1, and one validation label (queued or
    // already running, the job may start at once).
    let c = lab.card_of(&id).await;
    assert_eq!(c["column"], "validating");
    assert_eq!(c["assignee"], lab.w1.as_str(), "{c}");
    assert_eq!(by_of(&c), ["by:w1"], "{c}");
    assert_eq!(validation_of(&c).len(), 1, "{c}");
    // The pass: no validation label, the assignee cleared, one by:w1.
    let c = lab
        .until_card(&id, "review", |c| c["column"] == "review")
        .await;
    assert!(c["assignee"].is_null(), "{c}");
    assert!(validation_of(&c).is_empty(), "{c}");
    assert_eq!(by_of(&c), ["by:w1"], "{c}");
}

/// TER-107: a project admin who queues another agent's card keeps the
/// card's worker in `by:`. The job and the final mail go to that worker.
#[tokio::test]
async fn an_admin_validate_keeps_the_workers_by_label() {
    let lab = lab(2).await;
    let head = lab.branch("ter-admin", "next", &[("OK", "yes\n")]);
    let id = lab.card("admin queues", "ter-admin").await;
    // The worker's `by:` and a stale validation label, then the admin queues.
    lab.td
        .patch(
            &format!("/api/cards/{id}"),
            json!({"add_labels": ["by:w1", "validation:running"]}),
        )
        .await
        .ok();
    let seeded = lab.card_of(&id).await;
    assert!(labels(&seeded).contains(&"by:w1".to_owned()), "{seeded}");
    let r = lab
        .td
        .post_as(
            &lab.lead,
            &format!("/api/cards/{id}/validate"),
            json!({"commit": head}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["job"]["by_agent"], "w1", "{}", r.body);
    let by_of = |c: &Value| -> Vec<String> {
        labels(c)
            .into_iter()
            .filter(|l| l.starts_with("by:"))
            .collect()
    };
    let c = lab.card_of(&id).await;
    assert_eq!(by_of(&c), ["by:w1"], "{c}");
    let validation = labels(&c)
        .into_iter()
        .filter(|l| l.starts_with("validation:"))
        .count();
    assert_eq!(validation, 1, "{c}");
    let c = lab
        .until_card(&id, "review", |c| c["column"] == "review")
        .await;
    assert_eq!(by_of(&c), ["by:w1"], "{c}");
    assert!(
        !labels(&c).iter().any(|l| l.contains("lead")),
        "the admin is not a worker label: {c}"
    );
}

#[tokio::test]
async fn a_red_validation_runs_twice_then_goes_to_the_top_of_ready() {
    let lab = lab(2).await;
    // No OK file: the recipe fails.
    let head = lab.branch("ter-b", "next", &[("other", "x\n")]);
    let first = lab
        .td
        .post(
            "/api/projects/p/cards",
            json!({"title": "first in ready", "column": "ready"}),
        )
        .await
        .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let id = lab.card("red", "ter-b").await;
    let r = lab
        .td
        .post_as(
            &lab.w1,
            &format!("/api/cards/{id}/validate"),
            json!({"commit": head}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let c = lab
        .until_card(&id, "ready", |c| c["column"] == "ready")
        .await;
    let body = c["body"].as_str().unwrap();
    assert!(body.starts_with("Validation failed: "), "{body}");
    assert!(body.contains(&head), "{body}");
    assert!(body.contains("outcome red"), "{body}");
    assert!(body.contains("attempt 2"), "{body}");
    assert!(body.ends_with("the card body"), "{body}");
    assert!(c["assignee"].is_null(), "{c}");
    // At the top of ready.
    let b = lab.td.get("/api/projects/p/board").await;
    let ready: Vec<&str> = b.ok()["cards"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["column"] == "ready")
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    let first_pos = |x: &str| ready.iter().position(|c| *c == x).unwrap();
    assert!(first_pos(&id) < first_pos(&first), "{ready:?}");
    // Two tries, the second at the front of the queue.
    let jobs = lab.until_idle().await;
    let tries: Vec<i64> = jobs
        .iter()
        .filter(|j| j["card"] == id.as_str())
        .map(|j| j["attempt"].as_i64().unwrap())
        .collect();
    assert_eq!(tries.len(), 2, "{jobs:?}");
    assert!(jobs.iter().all(|j| j["state"] == "failed"), "{jobs:?}");
    // The worker gets a mail.
    let start = Instant::now();
    loop {
        let mail = lab.td.mail_of(&lab.w1).await;
        if mail.iter().any(|m| {
            m["subject"]
                .as_str()
                .unwrap_or("")
                .contains("validation failed")
        }) {
            break;
        }
        assert!(start.elapsed() < DEADLINE, "no mail to w1: {mail:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// TER-135: the stats give the first-attempt pass rate, the reruns, and
/// the failed test targets from the output tail.
#[tokio::test]
async fn the_stats_give_the_first_attempt_pass_rate_and_the_failed_targets() {
    let lab = lab(2).await;
    set_recipe(
        &lab,
        "test -f OK || { printf 'error: 1 target failed:\\n    `-p app --test flaky`\\n'; exit 101; }",
    )
    .await;
    let green = lab.branch("ter-g", "next", &[("OK", "yes\n")]);
    let red = lab.branch("ter-r", "next", &[("other", "x\n")]);
    for (title, branch, head) in [("green", "ter-g", &green), ("red", "ter-r", &red)] {
        let id = lab.card(title, branch).await;
        let r = lab
            .td
            .post_as(
                &lab.w1,
                &format!("/api/cards/{id}/validate"),
                json!({"commit": head}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
    }
    // The green job, and the red job with its one rerun.
    let start = Instant::now();
    while lab.until_idle().await.len() < 3 {
        assert!(start.elapsed() < DEADLINE, "no rerun");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    lab.until_idle().await;
    let r = lab
        .td
        .get_as(&lab.w1, "/api/projects/p/jobs/stats?since=1h")
        .await;
    let s = r.ok();
    assert_eq!(s["jobs"], 3, "{s}");
    assert_eq!(s["first_attempt"]["ended"], 2, "{s}");
    assert_eq!(s["first_attempt"]["passed"], 1, "{s}");
    assert_eq!(s["first_attempt"]["pass_rate"], 0.5, "{s}");
    assert_eq!(s["validate"]["ended"], 2, "{s}");
    assert_eq!(s["reruns"]["ended"], 1, "{s}");
    assert_eq!(s["reruns"]["passed"], 0, "{s}");
    assert_eq!(s["errors"], 0, "{s}");
    assert_eq!(s["untargeted_test_fails"], 0, "{s}");
    assert_eq!(
        s["failed_targets"],
        json!([{"target": "-p app --test flaky", "failed": 2, "first_attempt": 1}]),
        "{s}"
    );
    // The default window is 7 days. A time in the future has no jobs.
    assert_eq!(
        lab.td.get("/api/projects/p/jobs/stats").await.ok()["jobs"],
        3
    );
    let r = lab
        .td
        .get("/api/projects/p/jobs/stats?since=2999-01-01T00:00:00Z")
        .await;
    assert_eq!(r.ok()["jobs"], 0);
    assert!(r.ok()["first_attempt"]["pass_rate"].is_null());
    lab.td
        .get("/api/projects/p/jobs/stats?since=soon")
        .await
        .err(400, "bad_request");
}

#[tokio::test]
async fn a_merge_validates_and_pushes_next() {
    let lab = lab(2).await;
    let head = lab.branch("ter-c", "next", &[("OK", "yes\n")]);
    let id = lab.card("merge me", "ter-c").await;
    // Only an admin queues a merge.
    lab.td
        .post_as(&lab.w1, &format!("/api/cards/{id}/merge"), json!({}))
        .await
        .err(403, "forbidden");
    let r = lab
        .td
        .post_as(&lab.lead, &format!("/api/cards/{id}/merge"), json!({}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["job"]["kind"], "merge");
    lab.until_card(&id, "accepted", |c| c["column"] == "accepted")
        .await;
    // `next` on the remote holds the card commit, in a merge commit.
    let next = git(&lab.origin, &["rev-parse", "next"]);
    assert_ne!(next, head);
    git(&lab.origin, &["merge-base", "--is-ancestor", &head, "next"]);
    let parents = git(&lab.origin, &["rev-list", "--parents", "-n", "1", "next"]);
    assert_eq!(parents.split_whitespace().count(), 3, "{parents}");
    let jobs = lab.until_idle().await;
    assert_eq!(jobs[0]["head"].as_str().unwrap(), next);
}

#[tokio::test]
async fn a_conflict_is_not_resolved_and_not_rerun() {
    let lab = lab(2).await;
    // Two branches change the same line of README.
    lab.branch("ter-d1", "next", &[("README", "d1\n"), ("OK", "y\n")]);
    git(&lab.work, &["push", "-q", "origin", "ter-d1:next"]);
    let before = git(&lab.origin, &["rev-parse", "next"]);
    lab.branch("ter-d2", "main", &[("README", "d2\n"), ("OK", "y\n")]);
    let id = lab.card("conflict", "ter-d2").await;
    let r = lab
        .td
        .post(&format!("/api/cards/{id}/merge"), json!({}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let c = lab
        .until_card(&id, "ready", |c| c["column"] == "ready")
        .await;
    let body = c["body"].as_str().unwrap();
    assert!(body.starts_with("Validation failed: "), "{body}");
    assert!(body.contains("outcome conflict"), "{body}");
    assert!(body.contains("README"), "{body}");
    let jobs = lab.until_idle().await;
    assert_eq!(jobs.len(), 1, "a conflict is not rerun: {jobs:?}");
    assert_eq!(jobs[0]["outcome"], "conflict");
    // `next` did not move.
    assert_eq!(git(&lab.origin, &["rev-parse", "next"]), before);
}

#[tokio::test]
async fn jobs_keep_to_the_slots_in_order() {
    let lab = lab(1).await;
    // The recipe takes a moment, so the jobs would overlap without slots.
    lab.td
        .put(
            "/api/projects/p/delivery",
            json!({"target": "p-box", "recipe": "test -f OK && sleep 1", "slots": 1,
                   "next_repos": ["app"],
                   "work_dir": lab._dirs.path().join("jobs").display().to_string()}),
        )
        .await
        .ok();
    let mut cards = Vec::new();
    for n in ["e1", "e2", "e3"] {
        let head = lab.branch(&format!("ter-{n}"), "next", &[("OK", n)]);
        let id = lab.card(n, &format!("ter-{n}")).await;
        let r = lab
            .td
            .post_as(
                &lab.w1,
                &format!("/api/cards/{id}/validate"),
                json!({"commit": head}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
        cards.push(id);
    }
    // While they run, never more than one runs.
    let start = Instant::now();
    loop {
        let v = lab.td.get("/api/projects/p/jobs").await.ok().clone();
        assert!(v["running"].as_i64().unwrap() <= 1, "{v}");
        if v["running"] == 0 && v["queued"] == 0 {
            break;
        }
        assert!(start.elapsed() < DEADLINE, "the queue never drained: {v}");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    // First in, first out, with no overlap.
    let mut jobs = lab.jobs().await;
    jobs.sort_by(|a, b| {
        a["started_at"]
            .as_str()
            .unwrap()
            .cmp(b["started_at"].as_str().unwrap())
    });
    let order: Vec<&str> = jobs.iter().map(|j| j["card"].as_str().unwrap()).collect();
    assert_eq!(order, cards.iter().map(String::as_str).collect::<Vec<_>>());
    for w in jobs.windows(2) {
        assert!(
            w[1]["started_at"].as_str().unwrap() >= w[0]["ended_at"].as_str().unwrap(),
            "{w:?}"
        );
    }
    assert!(jobs.iter().all(|j| j["state"] == "passed"), "{jobs:?}");
    for id in &cards {
        assert_eq!(lab.card_of(id).await["column"], "review");
    }
}

#[tokio::test]
async fn a_cancel_ends_a_queued_job() {
    let lab = lab(1).await;
    lab.td
        .put(
            "/api/projects/p/delivery",
            json!({"target": "p-box", "recipe": "sleep 30", "slots": 1}),
        )
        .await
        .ok();
    let h1 = lab.branch("ter-f1", "next", &[("OK", "1")]);
    let h2 = lab.branch("ter-f2", "next", &[("OK", "2")]);
    let c1 = lab.card("f1", "ter-f1").await;
    let c2 = lab.card("f2", "ter-f2").await;
    let j1 = lab
        .td
        .post_as(
            &lab.w1,
            &format!("/api/cards/{c1}/validate"),
            json!({"commit": h1}),
        )
        .await
        .ok()["job"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let r = lab
        .td
        .post_as(
            &lab.w1,
            &format!("/api/cards/{c2}/validate"),
            json!({"commit": h2}),
        )
        .await;
    assert_eq!(r.body["queue"]["position"], 1, "{}", r.body);
    let j2 = r.body["job"]["id"].as_str().unwrap().to_owned();
    // A card has one open job at a time.
    lab.td
        .post(&format!("/api/cards/{c2}/validate"), json!({}))
        .await
        .err(409, "conflict");
    // The worker cancels its queued job; then the running one (killed).
    let r = lab.td.delete(&format!("/api/jobs/{j2}")).await;
    assert_eq!(r.ok()["state"], "cancelled");
    let start = Instant::now();
    while lab.td.get(&format!("/api/jobs/{j1}")).await.ok()["state"] != "running" {
        assert!(start.elapsed() < DEADLINE, "job 1 never started");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let r = lab.td.delete(&format!("/api/jobs/{j1}")).await;
    assert_eq!(r.ok()["state"], "cancelled");
    let jobs = lab.until_idle().await;
    assert!(jobs.iter().all(|j| j["state"] == "cancelled"), "{jobs:?}");
    // The cards stay where they are, without the job labels.
    let c = lab.card_of(&c2).await;
    assert_eq!(c["column"], "validating");
    assert!(!labels(&c).iter().any(|l| l.starts_with("validation:")));
}

/// TER-97: the host cap counts the jobs of all projects on one host. `p`
/// and `q` each have 2 slots, on targets of the host agent `lab1`, with a
/// cap of 1 on `lab1`: never more than one job runs, and the jobs start in
/// the order they were queued, across the projects. The recipe checks that
/// it runs 19 steps nicer than this process (at most 19), and in the idle
/// IO class when the machine has a working `ionice -c3`.
#[tokio::test]
async fn the_host_cap_holds_across_projects_in_order() {
    let lab = lab(2).await;
    let host_id = lab.td.get("/api/hosts/lab1").await.body["id"].clone();
    let wd = lab._dirs.path().join("jobs").display().to_string();
    // `nice -n19` adds 19 to the niceness of the host agent (this test
    // process), at most 19: a run under a parent at -4 gets 15.
    let base: i32 = String::from_utf8(std::process::Command::new("nice").output().unwrap().stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let want = (base + 19).min(19);
    let recipe = format!(
        "test -f OK && test \"$(nice)\" = {want} \
         && {{ ! ionice -c3 true 2>/dev/null || ionice -p $$ | grep -q idle; }} \
         && sleep 1"
    );
    let recipe = recipe.as_str();
    lab.td
        .put(
            "/api/projects/p/delivery",
            json!({"target": "p-box", "recipe": recipe, "slots": 2, "work_dir": wd}),
        )
        .await
        .ok();
    let r = lab
        .td
        .patch(
            "/api/projects/q/board",
            json!({"columns": [
                {"name": "backlog"}, {"name": "ready"},
                {"name": "in_progress", "role": "working"}, {"name": "validating"},
                {"name": "review"}, {"name": "accepted"},
                {"name": "blocked", "role": "blocked"}, {"name": "done"}]}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let r = lab
        .td
        .post(
            "/api/projects/q/repos",
            json!({"url": lab.origin.display().to_string(), "name": "app"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    lab.td
        .put(
            "/api/projects/q/delivery",
            json!({"target": "q-box", "recipe": recipe, "slots": 2, "work_dir": wd}),
        )
        .await
        .ok();

    // Only a sysadmin or the operator sets a cap; 0 is not a cap.
    lab.td
        .put_as(&lab.lead, "/api/job-hosts/lab1", json!({"heavy_jobs": 1}))
        .await
        .err(403, "forbidden");
    lab.td
        .put("/api/job-hosts/lab1", json!({"heavy_jobs": 0}))
        .await
        .err(422, "invalid");
    let r = lab
        .td
        .put("/api/job-hosts/lab1", json!({"heavy_jobs": 1}))
        .await;
    assert_eq!(r.ok()["hosts"][0]["host"], host_id, "{}", r.body);
    assert_eq!(r.ok()["hosts"][0]["heavy_jobs"], 1, "{}", r.body);
    let audit = lab
        .td
        .get("/api/audit?action=job.host_cap&order=desc")
        .await;
    assert_eq!(audit.ok()[0]["detail"]["new"], 1, "{}", audit.body);

    // Queue p, q, p, q: the operator queues (a project admin of both).
    let mut cards = Vec::new();
    for (n, project) in [("h1", "p"), ("h2", "q"), ("h3", "p"), ("h4", "q")] {
        let branch = format!("ter-{n}");
        let head = lab.branch(&branch, "next", &[("OK", n)]);
        let r = lab
            .td
            .post(
                &format!("/api/projects/{project}/cards"),
                json!({"title": n, "branch": branch, "column": "backlog"}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
        let id = r.body["id"].as_str().unwrap().to_owned();
        let r = lab
            .td
            .post(
                &format!("/api/cards/{id}/validate"),
                json!({"commit": head, "repo": "app"}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
        assert_eq!(r.body["queue"]["host"]["host"], host_id, "{}", r.body);
        assert_eq!(r.body["queue"]["host"]["heavy_jobs"], 1, "{}", r.body);
        cards.push(id);
    }

    // While they run, never more than one runs on lab1.
    let all = |v: &Value| v.as_i64().unwrap();
    let start = Instant::now();
    loop {
        let p = lab.td.get("/api/projects/p/jobs").await.ok().clone();
        let q = lab.td.get("/api/projects/q/jobs").await.ok().clone();
        let h = lab.td.get("/api/job-hosts").await.ok().clone();
        let running = all(&p["running"]) + all(&q["running"]);
        assert!(running <= 1, "p {p}\nq {q}");
        assert!(h["hosts"][0]["running"].as_i64().unwrap() <= 1, "{h}");
        if running == 0 && all(&p["queued"]) + all(&q["queued"]) == 0 {
            break;
        }
        assert!(
            start.elapsed() < DEADLINE,
            "the queues never drained: p {p}\nq {q}"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    // First in, first out across the projects, with no overlap.
    let mut jobs: Vec<Value> = Vec::new();
    for project in ["p", "q"] {
        let v = lab
            .td
            .get(&format!("/api/projects/{project}/jobs"))
            .await
            .ok()
            .clone();
        jobs.extend(v["jobs"].as_array().unwrap().iter().cloned());
    }
    assert!(
        jobs.iter()
            .all(|j| j["state"] == "passed" && j["host"] == host_id),
        "{jobs:#?}"
    );
    jobs.sort_by(|a, b| {
        a["started_at"]
            .as_str()
            .unwrap()
            .cmp(b["started_at"].as_str().unwrap())
    });
    let order: Vec<&str> = jobs.iter().map(|j| j["card"].as_str().unwrap()).collect();
    assert_eq!(order, cards.iter().map(String::as_str).collect::<Vec<_>>());
    for w in jobs.windows(2) {
        assert!(
            w[1]["started_at"].as_str().unwrap() >= w[0]["ended_at"].as_str().unwrap(),
            "{w:?}"
        );
    }
    // The wrapper is on the job.
    let id = jobs[0]["id"].as_str().unwrap();
    let j = lab.td.get(&format!("/api/jobs/{id}")).await.ok().clone();
    let tail = j["tail"].as_str().unwrap();
    assert!(tail.contains("nice -n19 sh -c <recipe>]"), "{tail}");

    // The cap goes away: the host has no cap and no running job.
    let r = lab.td.delete("/api/job-hosts/lab1").await;
    assert_eq!(r.ok()["hosts"], json!([]), "{}", r.body);
}

/// Queue a merge of a new card on `branch` with `recipe`, wait for the
/// end, and return the job.
async fn merge_with_recipe(lab: &Lab, branch: &str, recipe: &str) -> Value {
    lab.td
        .put(
            "/api/projects/p/delivery",
            json!({"target": "p-box", "recipe": recipe, "next_repos": ["app"],
                   "work_dir": lab._dirs.path().join("jobs").display().to_string()}),
        )
        .await
        .ok();
    lab.branch(branch, "next", &[("OK", "yes\n")]);
    let id = lab.card(branch, branch).await;
    let r = lab
        .td
        .post_as(&lab.lead, &format!("/api/cards/{id}/merge"), json!({}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let jobs = lab.until_idle().await;
    jobs.into_iter().find(|j| j["card"] == id.as_str()).unwrap()
}

/// Review B1: the recipe commits in its clone and moves HEAD. The push
/// sends the merge commit that the prepare step recorded, never the
/// recipe's commit.
#[tokio::test]
async fn a_merge_pushes_the_recorded_commit_not_the_head_after_the_recipe() {
    let lab = lab(2).await;
    let before = git(&lab.origin, &["rev-parse", "next"]);
    let job = merge_with_recipe(
        &lab,
        "ter-m1",
        "test -f OK && echo evil > EVIL && git add EVIL \
         && git -c user.name=x -c user.email=x@x commit -q -m evil \
         && git checkout -q -B other && test \"$(git log -1 --format=%s)\" = evil",
    )
    .await;
    assert_eq!(job["state"], "passed", "{job:#}");
    let next = git(&lab.origin, &["rev-parse", "next"]);
    assert_ne!(next, before);
    assert_eq!(job["head"].as_str().unwrap(), next, "{job:#}");
    assert_eq!(
        git(&lab.origin, &["log", "-1", "--format=%s", "next"]),
        "Merge ter-m1 onto next"
    );
    let files = git(&lab.origin, &["ls-tree", "--name-only", "next"]);
    assert!(!files.lines().any(|f| f == "EVIL"), "{files}");
    let all = git(&lab.origin, &["log", "--all", "--format=%s"]);
    assert!(!all.lines().any(|l| l == "evil"), "{all}");
}

/// Review B2: the recipe plants `url.<x>.insteadOf` and `pushInsteadOf`
/// in its clone's `.git/config` to send the push (and the credential) to
/// another repo. The push does not read that config: `next` goes to the
/// project's repo, and the other repo gets nothing.
#[tokio::test]
async fn a_merge_push_ignores_the_config_that_the_recipe_wrote() {
    let lab = lab(2).await;
    let evil = lab._dirs.path().join("evil.git");
    std::fs::create_dir_all(&evil).unwrap();
    git(&evil, &["init", "-q", "--bare", "-b", "main"]);
    let (origin, evil_s) = (lab.origin.display().to_string(), evil.display().to_string());
    let recipe = format!(
        "test -f OK && git config 'url.{evil_s}.insteadOf' '{origin}' \
         && git config 'url.{evil_s}.pushInsteadOf' '{origin}' \
         && git config remote.pushDefault evil && git config remote.evil.url '{evil_s}'"
    );
    let job = merge_with_recipe(&lab, "ter-m2", &recipe).await;
    assert_eq!(job["state"], "passed", "{job:#}");
    assert_eq!(
        job["head"].as_str().unwrap(),
        git(&lab.origin, &["rev-parse", "next"])
    );
    assert_eq!(
        git(&evil, &["for-each-ref"]),
        "",
        "the other repo got a push"
    );
}

/// TER-159: merges have their own lane. With 1 slot, a validate job queued
/// after a merge starts beside it and passes while the merge runs, and a
/// second merge waits for the first.
#[tokio::test]
async fn a_validation_passes_while_a_merge_runs() {
    let lab = lab(1).await;
    lab.td
        .put(
            "/api/projects/p/delivery",
            json!({"target": "p-box", "recipe": "test -f OK && sleep 2", "slots": 1,
                   "next_repos": ["app"],
                   "work_dir": lab._dirs.path().join("jobs").display().to_string()}),
        )
        .await
        .ok();
    lab.branch("ter-m1", "next", &[("OK", "1")]);
    let hv = lab.branch("ter-v", "next", &[("OK", "2")]);
    // Its own file: it merges onto the `next` of the first merge, which
    // holds `OK`.
    lab.branch("ter-m2", "next", &[("M2", "3")]);
    let cm1 = lab.card("m1", "ter-m1").await;
    let cv = lab.card("v", "ter-v").await;
    let cm2 = lab.card("m2", "ter-m2").await;
    let r = lab
        .td
        .post_as(&lab.lead, &format!("/api/cards/{cm1}/merge"), json!({}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["queue"]["slots"], 1, "{}", r.body);
    let r = lab
        .td
        .post_as(&lab.lead, &format!("/api/cards/{cm2}/merge"), json!({}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    // Behind the first merge, queued or running.
    let q = &r.body["queue"];
    assert_eq!(
        q["position"].as_i64().unwrap() + q["running"].as_i64().unwrap(),
        2,
        "{}",
        r.body
    );
    let r = lab
        .td
        .post_as(
            &lab.w1,
            &format!("/api/cards/{cv}/validate"),
            json!({"commit": hv}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    // The merges do not count against the one slot.
    assert_eq!(r.body["queue"]["position"], 1, "{}", r.body);
    assert_eq!(r.body["queue"]["running"], 0, "{}", r.body);
    let jobs = lab.until_idle().await;
    let of = |c: &str| jobs.iter().find(|j| j["card"] == c).unwrap().clone();
    let (m1, v, m2) = (of(&cm1), of(&cv), of(&cm2));
    for j in [&m1, &v, &m2] {
        assert_eq!(j["state"], "passed", "{jobs:#?}");
    }
    let t = |j: &Value, k: &str| j[k].as_str().unwrap().to_owned();
    // The validate job ran while a merge ran.
    let overlaps = |m: &Value| {
        t(&v, "started_at") < t(m, "ended_at") && t(m, "started_at") < t(&v, "ended_at")
    };
    assert!(overlaps(&m1) || overlaps(&m2), "{jobs:#?}");
    // One merge at a time, in order.
    assert!(t(&m2, "started_at") >= t(&m1, "ended_at"), "{jobs:#?}");
}

// ------------------------------------------------------------- TER-115

/// Is `pid` a live process (not gone, not a zombie)?
fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .is_ok_and(|s| !s.rsplit(") ").next().unwrap_or("").starts_with('Z'))
}

/// The PIDs in `file`, one per line, once there are at least `n`.
async fn pids(file: &Path, n: usize) -> Vec<u32> {
    let start = Instant::now();
    loop {
        let p: Vec<u32> = std::fs::read_to_string(file)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect();
        if p.len() >= n {
            return p;
        }
        assert!(
            start.elapsed() < DEADLINE,
            "no {n} pids in {}",
            file.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A fake `docker` whose `exec` runs the command in a new session on
/// this machine, as runc does in a container: a kill of the `docker`
/// client (the host agent's timeout and kill) does not reach it.
fn fake_docker(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let docker = dir.join("docker");
    std::fs::write(
        &docker,
        r#"#!/bin/sh
case "$1" in
  inspect) echo '[{"Id":"abc","Name":"/terrarium-p-dbox","State":{"Status":"running"}}]' ;;
  run) echo abc ;;
  exec) shift
    while [ $# -gt 0 ]; do case "$1" in -i|-t) shift ;; -w) cd "$2" || exit 1; shift 2 ;; -e|-u) shift 2 ;; *) break ;; esac; done
    shift
    ( exec setsid "$@" ) &
    wait $! ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
    docker
}

/// A lab whose queue runs on the docker target `p-dbox` (fake docker), with
/// a recipe that starts a child, ignores `TERM` (the child too), and
/// appends the child's PID to the returned file.
async fn docker_lab(slots: u32, timeout_secs: u64) -> (Lab, tempfile::TempDir, PathBuf) {
    let bins = tempfile::tempdir().unwrap();
    let docker = fake_docker(bins.path());
    let lab = lab_with(slots, Some(&docker)).await;
    let r = lab
        .td
        .post(
            "/api/targets",
            json!({"name": "p-dbox", "kind": "docker", "host": "lab1", "image": "alpine:3",
                   "project": "p"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let child_file = bins.path().join("children");
    let recipe = format!(
        "trap '' TERM; sleep 300 & echo $! >> '{}'; wait",
        child_file.display()
    );
    let work_dir = bins.path().join("jobs");
    let r = lab
        .td
        .put(
            "/api/projects/p/delivery",
            json!({"target": "p-dbox", "recipe": recipe, "slots": slots,
                   "timeout_secs": timeout_secs,
                   "work_dir": work_dir.display().to_string()}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    (lab, bins, child_file)
}

/// Poll job `id` until its state is not `state`; return it.
async fn until_not(lab: &Lab, id: &str, state: &str) -> Value {
    let start = Instant::now();
    loop {
        let j = lab.td.get(&format!("/api/jobs/{id}")).await.ok().clone();
        if j["state"] != state {
            return j;
        }
        assert!(start.elapsed() < DEADLINE, "job {id} stays {state}: {j}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// TER-115: a recipe on a docker target (its command in its own session,
/// as in a container) starts a child that ignores `TERM`, and the job
/// hits its timeout. The job ends only after the kill on the target: when
/// it shows as ended, no process of it is left. The rerun does the same.
#[tokio::test]
async fn a_job_timeout_ends_every_process_of_the_recipe() {
    let (lab, _bins, children) = docker_lab(1, 3).await;
    let h = lab.branch("ter-t1", "next", &[("OK", "1")]);
    let c = lab.card("t1", "ter-t1").await;
    let r = lab
        .td
        .post_as(
            &lab.w1,
            &format!("/api/cards/{c}/validate"),
            json!({"commit": h}),
        )
        .await;
    let j1 = r.ok()["job"]["id"].as_str().unwrap().to_owned();
    let first = pids(&children, 1).await[0];
    assert!(alive(first));
    let j = until_not(&lab, &j1, "running").await;
    assert!(!alive(first), "the child {first} outlived its job: {j}");
    assert_eq!(j["state"], "failed", "{j}");
    assert_eq!(j["outcome"], "red", "{j}");
    let tail = j["tail"].as_str().unwrap_or("");
    assert!(tail.contains("timeout"), "{j}");
    assert!(tail.contains("killed on the target"), "{j}");
    // The rerun times out the same way.
    let jobs = lab.until_idle().await;
    assert_eq!(jobs.len(), 2, "{jobs:?}");
    for p in pids(&children, 2).await {
        assert!(!alive(p), "the child {p} outlived its job");
    }
}

/// TER-115: a cancel of a running job ends every process of its recipe on
/// the target, and the next job starts only after they are gone.
#[tokio::test]
async fn a_cancel_ends_every_process_before_the_next_job_starts() {
    let (lab, _bins, children) = docker_lab(1, 600).await;
    let h1 = lab.branch("ter-k1", "next", &[("OK", "1")]);
    let h2 = lab.branch("ter-k2", "next", &[("OK", "2")]);
    let c1 = lab.card("k1", "ter-k1").await;
    let c2 = lab.card("k2", "ter-k2").await;
    let mut ids = Vec::new();
    for (c, h) in [(&c1, &h1), (&c2, &h2)] {
        let r = lab
            .td
            .post_as(
                &lab.w1,
                &format!("/api/cards/{c}/validate"),
                json!({"commit": h}),
            )
            .await;
        ids.push(r.ok()["job"]["id"].as_str().unwrap().to_owned());
    }
    let first = pids(&children, 1).await[0];
    assert!(alive(first));
    let r = lab.td.delete(&format!("/api/jobs/{}", ids[0])).await;
    assert_eq!(r.ok()["state"], "cancelled");
    let j2 = until_not(&lab, &ids[1], "queued").await;
    assert!(
        !alive(first),
        "job 2 started while the child {first} of job 1 ran: {j2}"
    );
    // Job 2's recipe runs; cancel it too.
    let all = pids(&children, 2).await;
    let r = lab.td.delete(&format!("/api/jobs/{}", ids[1])).await;
    assert_eq!(r.ok()["state"], "cancelled");
    for p in all {
        let start = Instant::now();
        while alive(p) {
            assert!(start.elapsed() < DEADLINE, "the child {p} outlived its job");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// TER-115 review (round 2, minor 2): a merge job that gets a cancel
/// between two steps starts no more steps, so it does not push `next`.
/// A fake `ionice` holds the first ionice probe (the step before the
/// prepare step) until the kill. The cancel comes during the probe, so
/// the kill ends only the probe, and only the killed() check keeps the
/// prepare, recipe, collect, and push steps from running. A validate job
/// behind it (one slot) starts only after the merge job's run ended.
#[tokio::test]
async fn a_cancelled_merge_job_does_not_push() {
    use std::os::unix::fs::PermissionsExt;
    let bins = tempfile::tempdir().unwrap();
    let docker = fake_docker(bins.path());
    let script = std::fs::read_to_string(&docker).unwrap().replacen(
        "#!/bin/sh\n",
        &format!(
            "#!/bin/sh\nPATH='{}':\"$PATH\"; export PATH\n",
            bins.path().display()
        ),
        1,
    );
    std::fs::write(&docker, script).unwrap();
    let probe = bins.path().join("probe");
    let ionice = bins.path().join("ionice");
    std::fs::write(
        &ionice,
        format!(
            "#!/bin/sh\nif [ ! -e '{p}' ]; then echo $$ > '{p}'; exec sleep 300; fi\nshift\nexec \"$@\"\n",
            p = probe.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&ionice, std::fs::Permissions::from_mode(0o755)).unwrap();
    let lab = lab_with(1, Some(&docker)).await;
    // A target name of its own: the ionice answer is kept per target name
    // in the process, and this test needs the probe to run.
    let r = lab
        .td
        .post(
            "/api/targets",
            json!({"name": "p-ibox", "kind": "docker", "host": "lab1", "image": "alpine:3",
                   "project": "p"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let ran = bins.path().join("ran");
    let work_dir = bins.path().join("jobs");
    let r = lab
        .td
        .put(
            "/api/projects/p/delivery",
            json!({"target": "p-ibox", "slots": 1, "next_repos": ["app"],
                   "recipe": format!("echo \"$PWD\" >> '{}'; test -f OK", ran.display()),
                   "work_dir": work_dir.display().to_string()}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let before = git(&lab.origin, &["rev-parse", "next"]);
    lab.branch("ter-mc", "next", &[("OK", "yes\n")]);
    let card = lab.card("mc", "ter-mc").await;
    let r = lab
        .td
        .post_as(&lab.lead, &format!("/api/cards/{card}/merge"), json!({}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let job = r.body["job"]["id"].as_str().unwrap().to_owned();
    let held = pids(&probe, 1).await[0];
    assert!(alive(held));
    let r = lab.td.delete(&format!("/api/jobs/{job}")).await;
    assert_eq!(r.ok()["state"], "cancelled");
    let h2 = lab.branch("ter-mv", "next", &[("OK", "yes\n")]);
    let c2 = lab.card("mv", "ter-mv").await;
    let r = lab
        .td
        .post_as(
            &lab.w1,
            &format!("/api/cards/{c2}/validate"),
            json!({"commit": h2}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let v = r.body["job"]["id"].as_str().unwrap().to_owned();
    let jobs = lab.until_idle().await;
    let of = |id: &str| jobs.iter().find(|j| j["id"] == id).unwrap().clone();
    assert_eq!(of(&job)["state"], "cancelled", "{jobs:#?}");
    assert_eq!(of(&v)["state"], "passed", "{jobs:#?}");
    assert!(!alive(held), "the probe {held} outlived the kill");
    let log = std::fs::read_to_string(&ran).unwrap_or_default();
    assert!(
        !log.contains(&job),
        "the cancelled job ran its recipe: {log}"
    );
    assert!(log.contains(&v), "the validate job did not run: {log}");
    assert_eq!(git(&lab.origin, &["rev-parse", "next"]), before);
}

/// Set the recipe of project `p`, with the lab's work dir.
async fn set_recipe(lab: &Lab, recipe: &str) {
    lab.td
        .put(
            "/api/projects/p/delivery",
            json!({"target": "p-box", "recipe": recipe, "slots": 2, "next_repos": ["app"],
                   "work_dir": lab._dirs.path().join("jobs").display().to_string()}),
        )
        .await
        .ok();
}

/// The subjects of the merges on `next` of the remote, oldest first.
fn next_merges(lab: &Lab) -> Vec<String> {
    let log = git(
        &lab.origin,
        &["log", "--first-parent", "--reverse", "--format=%s", "next"],
    );
    log.lines()
        .filter(|l| l.starts_with("Merge "))
        .map(str::to_owned)
        .collect()
}

/// TER-112: one merge job takes three cards, merges them onto `next` in
/// the given order, validates once, pushes `next`, and moves each card to
/// `accepted`.
#[tokio::test]
async fn a_green_batch_of_three_merges_in_one_job() {
    let lab = lab(2).await;
    set_recipe(
        &lab,
        "test -f OK && test -f g1 && test -f g2 && test -f g3 && sleep 1",
    )
    .await;
    let mut heads = Vec::new();
    let mut cards = Vec::new();
    for n in ["g1", "g2", "g3"] {
        let b = format!("ter-{n}");
        heads.push(lab.branch(&b, "next", &[("OK", "yes\n"), (n, n)]));
        cards.push(lab.card(n, &b).await);
    }
    // Only an admin queues a batch, and a batch is checked.
    lab.td
        .post_as(&lab.w1, "/api/merges", json!({"cards": cards}))
        .await
        .err(403, "forbidden");
    lab.td
        .post_as(&lab.lead, "/api/merges", json!({"cards": []}))
        .await
        .err(422, "invalid");
    lab.td
        .post_as(
            &lab.lead,
            "/api/merges",
            json!({"cards": [cards[0], cards[0]]}),
        )
        .await
        .err(422, "invalid");
    // The first card is w1's: w1 still may not cancel the batch (review
    // F3), as it holds the other cards too.
    lab.td
        .patch(
            &format!("/api/cards/{}", cards[0]),
            json!({"add_labels": ["by:w1"]}),
        )
        .await
        .ok();
    let r = lab
        .td
        .post_as(&lab.lead, "/api/merges", json!({"cards": cards}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["job"]["kind"], "merge");
    assert_eq!(r.body["job"]["card"], cards[0].as_str());
    assert_eq!(r.body["job"]["by_agent"], "w1", "{}", r.body);
    let job_id = r.body["job"]["id"].as_str().unwrap().to_owned();
    lab.td
        .req(
            hyper::Method::DELETE,
            &format!("/api/jobs/{job_id}"),
            None,
            Some(&lab.w1),
        )
        .await
        .err(403, "forbidden");
    let batch: Vec<&str> = r.body["job"]["batch"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["card"].as_str().unwrap())
        .collect();
    assert_eq!(batch, cards.iter().map(String::as_str).collect::<Vec<_>>());
    // A card of the open batch takes no other job.
    lab.td
        .post_as(
            &lab.lead,
            &format!("/api/cards/{}/merge", cards[1]),
            json!({}),
        )
        .await
        .err(409, "conflict");
    // Each card shows the job, unless the job has ended already.
    for c in &cards {
        let card = lab.card_of(c).await;
        let l = labels(&card);
        assert!(
            l.iter().any(|l| l.starts_with("validation:")) || card["column"] == "accepted",
            "{c}: {l:?}"
        );
    }
    for c in &cards {
        let card = lab
            .until_card(c, "accepted", |c| c["column"] == "accepted")
            .await;
        let l = labels(&card);
        assert!(!l.iter().any(|l| l.starts_with("validation:")), "{l:?}");
        assert!(comments(&card).contains("batch of 3"), "{card}");
    }
    let jobs = lab.until_idle().await;
    assert_eq!(jobs.len(), 1, "one job for the batch: {jobs:#?}");
    assert_eq!(jobs[0]["state"], "passed");
    let next = git(&lab.origin, &["rev-parse", "next"]);
    assert_eq!(jobs[0]["head"].as_str().unwrap(), next);
    for h in &heads {
        git(&lab.origin, &["merge-base", "--is-ancestor", h, "next"]);
    }
    assert_eq!(
        next_merges(&lab),
        [
            "Merge ter-g1 onto next",
            "Merge ter-g2 onto next",
            "Merge ter-g3 onto next"
        ]
    );
}

/// TER-112: a red batch splits in halves until it finds the failing card.
/// The failing card goes to the top of `ready` with the failure block,
/// and the other cards merge, in order. The bad card is in the left half,
/// so the left half splits again and its halves must still run before
/// the right half (review F1, F2).
#[tokio::test]
async fn a_red_batch_finds_the_failing_card_and_the_others_merge() {
    let lab = lab(2).await;
    set_recipe(&lab, "test -f OK && test ! -f BAD").await;
    let h1 = lab.branch("ter-r1", "next", &[("OK", "yes\n"), ("r1", "1")]);
    let h2 = lab.branch("ter-r2", "next", &[("OK", "yes\n"), ("BAD", "2")]);
    let h3 = lab.branch("ter-r3", "next", &[("OK", "yes\n"), ("r3", "3")]);
    let h4 = lab.branch("ter-r4", "next", &[("OK", "yes\n"), ("r4", "4")]);
    let c1 = lab.card("r1", "ter-r1").await;
    let c2 = lab.card("r2", "ter-r2").await;
    let c3 = lab.card("r3", "ter-r3").await;
    let c4 = lab.card("r4", "ter-r4").await;
    let r = lab
        .td
        .post_as(&lab.lead, "/api/merges", json!({"cards": [c1, c2, c3, c4]}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let bad = lab
        .until_card(&c2, "ready", |c| c["column"] == "ready")
        .await;
    let body = bad["body"].as_str().unwrap();
    assert!(body.starts_with("Validation failed: "), "{body}");
    assert!(body.contains("outcome red"), "{body}");
    assert!(body.contains("ter-r2"), "{body}");
    assert!(body.ends_with("the card body"), "{body}");
    assert!(bad["assignee"].is_null(), "{bad}");
    for c in [&c1, &c3, &c4] {
        lab.until_card(c, "accepted", |c| c["column"] == "accepted")
            .await;
    }
    let jobs = lab.until_idle().await;
    // [r1 r2 r3 r4] red; [r1 r2] red, then [r1] green and [r2] red (no
    // rerun: its batch was red); then [r3 r4] green.
    assert_eq!(jobs.len(), 5, "{jobs:#?}");
    let state_of = |path: &str| -> String {
        let j = jobs
            .iter()
            .find(|j| j["split_path"].as_str().unwrap_or("") == path)
            .unwrap_or_else(|| panic!("no job at {path:?}: {jobs:#?}"));
        j["state"].as_str().unwrap().to_owned()
    };
    assert_eq!(state_of(""), "failed");
    assert_eq!(state_of("0"), "failed");
    assert_eq!(state_of("00"), "passed");
    assert_eq!(state_of("01"), "failed");
    assert_eq!(state_of("1"), "passed");
    assert!(
        jobs.iter()
            .filter(|j| j["outcome"] == "red")
            .all(|j| j["attempt"] == 1),
        "a card split out alone is not rerun: {jobs:#?}"
    );
    for h in [&h1, &h3, &h4] {
        git(&lab.origin, &["merge-base", "--is-ancestor", h, "next"]);
    }
    let out = std::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", &h2, "next"])
        .current_dir(&lab.origin)
        .status()
        .unwrap();
    assert!(!out.success(), "the red card is not on next");
    assert_eq!(
        next_merges(&lab),
        [
            "Merge ter-r1 onto next",
            "Merge ter-r3 onto next",
            "Merge ter-r4 onto next"
        ]
    );
    let c1v = lab.card_of(&c1).await;
    assert!(comments(&c1v).contains("The batch splits"), "{c1v}");
}

/// TER-112: a card of a batch that conflicts with the cards before it
/// leaves the batch at once (a conflict is never resolved or rerun); the
/// others validate and merge in the same job.
#[tokio::test]
async fn a_conflict_in_a_batch_drops_only_that_card() {
    let lab = lab(2).await;
    lab.branch("ter-k1", "next", &[("README", "k1\n"), ("OK", "y\n")]);
    lab.branch("ter-k2", "next", &[("README", "k2\n"), ("OK", "y\n")]);
    let h3 = lab.branch("ter-k3", "next", &[("k3", "3"), ("OK", "y\n")]);
    let c1 = lab.card("k1", "ter-k1").await;
    let c2 = lab.card("k2", "ter-k2").await;
    let c3 = lab.card("k3", "ter-k3").await;
    let r = lab
        .td
        .post_as(&lab.lead, "/api/merges", json!({"cards": [c1, c2, c3]}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let k2 = lab
        .until_card(&c2, "ready", |c| c["column"] == "ready")
        .await;
    let body = k2["body"].as_str().unwrap();
    assert!(body.starts_with("Validation failed: "), "{body}");
    assert!(body.contains("outcome conflict"), "{body}");
    assert!(body.contains("README"), "{body}");
    for c in [&c1, &c3] {
        lab.until_card(c, "accepted", |c| c["column"] == "accepted")
            .await;
    }
    let jobs = lab.until_idle().await;
    assert_eq!(jobs.len(), 1, "{jobs:#?}");
    assert_eq!(jobs[0]["state"], "passed");
    assert_eq!(jobs[0]["batch"][1]["outcome"], "conflict", "{jobs:#?}");
    git(&lab.origin, &["merge-base", "--is-ancestor", &h3, "next"]);
    assert_eq!(
        next_merges(&lab),
        ["Merge ter-k1 onto next", "Merge ter-k3 onto next"]
    );
}

/// TER-112 review F4: a branch whose merge fails without a conflict (an
/// unrelated history) leaves the batch alone; the others merge in the same
/// job.
#[tokio::test]
async fn a_merge_that_fails_without_a_conflict_drops_only_that_card() {
    let lab = lab(2).await;
    let h1 = lab.branch("ter-u1", "next", &[("u1", "1"), ("OK", "y\n")]);
    // An orphan branch: no history in common with `next`.
    git(&lab.work, &["checkout", "-q", "--orphan", "ter-u2"]);
    git(&lab.work, &["rm", "-rq", "--cached", "."]);
    std::fs::write(lab.work.join("u2"), "2").unwrap();
    git(&lab.work, &["add", "u2"]);
    git(&lab.work, &["commit", "-q", "-m", "orphan"]);
    git(&lab.work, &["push", "-q", "-f", "origin", "ter-u2"]);
    git(&lab.work, &["clean", "-fdq"]);
    let h3 = lab.branch("ter-u3", "next", &[("u3", "3"), ("OK", "y\n")]);
    let c1 = lab.card("u1", "ter-u1").await;
    let c2 = lab.card("u2", "ter-u2").await;
    let c3 = lab.card("u3", "ter-u3").await;
    let r = lab
        .td
        .post_as(&lab.lead, "/api/merges", json!({"cards": [c1, c2, c3]}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let u2 = lab
        .until_card(&c2, "ready", |c| c["column"] == "ready")
        .await;
    let body = u2["body"].as_str().unwrap();
    assert!(body.contains("outcome conflict"), "{body}");
    assert!(body.contains("without a conflict"), "{body}");
    for c in [&c1, &c3] {
        lab.until_card(c, "accepted", |c| c["column"] == "accepted")
            .await;
    }
    let jobs = lab.until_idle().await;
    assert_eq!(jobs.len(), 1, "{jobs:#?}");
    assert_eq!(jobs[0]["state"], "passed");
    for h in [&h1, &h3] {
        git(&lab.origin, &["merge-base", "--is-ancestor", h, "next"]);
    }
    assert_eq!(
        next_merges(&lab),
        ["Merge ter-u1 onto next", "Merge ter-u3 onto next"]
    );
}

/// Validate the card on `branch` with the current recipe and wait for
/// the end of its job.
async fn validate_card(lab: &Lab, branch: &str, head: &str) -> Value {
    let id = lab.card(branch, branch).await;
    let r = lab
        .td
        .post_as(
            &lab.w1,
            &format!("/api/cards/{id}/validate"),
            json!({"commit": head}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let jobs = lab.until_idle().await;
    jobs.into_iter().find(|j| j["card"] == id.as_str()).unwrap()
}

/// TER-100: the recipe gets the job kind, the head, the base, and the
/// file of the changed paths. A validate job diffs against the merge-base
/// with `next` (a later change on `next` is not in the list), a merge job
/// against `next` before the merge. Without `next`, no base and no list.
#[tokio::test]
async fn the_recipe_gets_the_job_kind_and_the_changed_files() {
    let lab = lab(2).await;
    let out = lab._dirs.path().join("env");
    std::fs::create_dir_all(&out).unwrap();
    let recipe = format!(
        "test -f OK && f='{}/'\"$TERRARIUM_JOB_KIND-$TERRARIUM_JOB_ID\" \
         && printf 'kind=%s\\nhead=%s\\nbase=%s\\nfiles=%s\\n' \"$TERRARIUM_JOB_KIND\" \
            \"$TERRARIUM_HEAD\" \"${{TERRARIUM_BASE-unset}}\" \"${{TERRARIUM_CHANGED_FILES-unset}}\" > \"$f\" \
         && if [ -n \"${{TERRARIUM_CHANGED_FILES:-}}\" ]; then cat \"$TERRARIUM_CHANGED_FILES\" >> \"$f\"; fi",
        out.display()
    );
    set_recipe(&lab, &recipe).await;
    let fork = git(&lab.origin, &["rev-parse", "next"]);
    let head = lab.branch("ter-env", "next", &[("OK", "yes\n"), ("a.rs", "a\n")]);
    // `next` moves after the fork: its change is not the card's.
    lab.branch("ter-other", "next", &[("other", "x\n")]);
    git(&lab.work, &["push", "-q", "origin", "ter-other:next"]);
    let moved = git(&lab.origin, &["rev-parse", "next"]);
    let read = |job: &Value| {
        let name = format!(
            "{}-{}",
            job["kind"].as_str().unwrap(),
            job["id"].as_str().unwrap()
        );
        std::fs::read_to_string(out.join(name)).unwrap()
    };

    let job = validate_card(&lab, "ter-env", &head).await;
    assert_eq!(job["state"], "passed", "{job:#}");
    let text = read(&job);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "kind=validate", "{text}");
    assert_eq!(lines[1], format!("head={head}"), "{text}");
    assert_eq!(lines[2], format!("base={fork}"), "{text}");
    assert!(
        lines[3].starts_with("files=/") && lines[3].ends_with("/.git/terrarium/changed-files"),
        "{text}"
    );
    assert_eq!(lines[4..], ["OK", "a.rs"], "{text}");

    let job = merge_with_recipe(&lab, "ter-env2", &recipe).await;
    assert_eq!(job["state"], "passed", "{job:#}");
    let text = read(&job);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "kind=merge", "{text}");
    assert_eq!(lines[2], format!("base={moved}"), "{text}");
    assert_eq!(lines[4..], ["OK"], "{text}");

    // No `next`: the job runs, without a base and a list.
    git(&lab.work, &["push", "-q", "origin", ":next"]);
    let job = validate_card(&lab, "ter-env", &head).await;
    assert_eq!(job["state"], "passed", "{job:#}");
    let text = read(&job);
    assert!(text.contains("base=unset\nfiles=unset\n"), "{text}");
    assert_eq!(text.lines().count(), 4, "{text}");
}
