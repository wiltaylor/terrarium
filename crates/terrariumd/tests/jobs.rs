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
    let audit = td.get("/api/audit?action=project.delivery&order=desc").await;
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
/// it runs at the lowest CPU priority (and the idle IO class, when the
/// machine has `ionice`).
#[tokio::test]
async fn the_host_cap_holds_across_projects_in_order() {
    let lab = lab(2).await;
    let wd = lab._dirs.path().join("jobs").display().to_string();
    let recipe = "test -f OK && test \"$(nice)\" = 19 \
                  && { ! ionice -c3 true 2>/dev/null || ionice -p $$ | grep -q idle; } \
                  && sleep 1";
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
    assert_eq!(r.ok()["hosts"][0]["host"], "lab1", "{}", r.body);
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
        assert_eq!(r.body["queue"]["host"]["host"], "lab1", "{}", r.body);
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
        assert!(start.elapsed() < DEADLINE, "the queues never drained: p {p}\nq {q}");
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
        jobs.iter().all(|j| j["state"] == "passed" && j["host"] == "lab1"),
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
