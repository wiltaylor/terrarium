//! The `run` grant level (TER-142): `terrarium run -e KEY=@secret:<name>`.
//! A real `terrarium_host::Client` runs the commands on this machine. The
//! daemon resolves the value at each run start, only on a target that the
//! grant names, puts it into the env of the remote process, and redacts
//! it from the session output, the run log, and the output tail. The
//! audit row names the secret, the target, and the run, never the value.
//! A `run` grant gives no `secret get` and no env var at the start.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use common::TestDaemon;
use serde_json::{Value, json};
use terrarium_core::protocol::HostCapabilities;
use terrarium_host::{Client, ClientOptions};

/// The secret `deploy` of project `p`: strong, so the daemon matcher has
/// it too.
const DEPLOY: &str = "dpl_8sK2mQ7vX4tR9wL3nB6yH1cJ5fZ0";
/// The password of the `login` secret `db` of `p`: weak, so the daemon
/// matcher leaves it out; only the run's own matcher redacts it.
const WEAK: &str = "hunter2pw";

struct Lab {
    td: TestDaemon,
    scripts: tempfile::TempDir,
    w1: String,
}

/// A daemon with the host agent `lab1` online, the host targets `box` and
/// `web`, agent `p/w1` with both in its policy, and the secrets `p/deploy`
/// and `p/db`.
async fn lab() -> Lab {
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
    let scripts = tempfile::tempdir().unwrap();
    o.paths = terrarium_host::HostPaths {
        work_dir: td.state.with_file_name("work"),
        instances_dir: scripts.path().join("instances"),
        repos_dir: scripts.path().join("repos"),
    };
    tokio::spawn(Client::new(o).run());
    for _ in 0..300 {
        if td.get("/api/hosts/lab1").await.body["online"] == true {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    for name in ["box", "web"] {
        let r = td
            .post(
                "/api/targets",
                json!({"name": name, "kind": "host", "host": "lab1"}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
    }
    td.project("p").await;
    let w1 = td
        .agent("p", "w1", json!({"policy": {"targets": ["box", "web"]}}))
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let r = td
        .post(
            "/api/secrets",
            json!({"scope": "p", "name": "deploy", "kind": "api_key",
                   "fields": {"value": DEPLOY}}),
        )
        .await;
    assert!(r.status.is_success(), "{}", r.body);
    let r = td
        .post(
            "/api/secrets",
            json!({"scope": "p", "name": "db", "kind": "login",
                   "fields": {"username": "app", "password": WEAK}}),
        )
        .await;
    assert!(r.status.is_success(), "{}", r.body);
    Lab { td, scripts, w1 }
}

impl Lab {
    /// A script in the scripts dir, mode 0755.
    fn script(&self, name: &str, body: &str) -> String {
        let p = self.scripts.path().join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.display().to_string()
    }

    async fn run(&self, target: &str, cmd: &str, env: Value) -> common::Resp {
        self.td
            .req(
                hyper::Method::POST,
                &format!("/api/agents/{}/run", self.w1),
                Some(json!({"target": target, "cmd": [cmd], "env": env, "name": "deploy"})),
                Some(&self.w1),
            )
            .await
    }

    /// Wait for the run's wait to end; the wait.
    async fn ended(&self, wait: &str) -> Value {
        for _ in 0..300 {
            let w = self
                .td
                .req(
                    hyper::Method::GET,
                    &format!("/api/waits/{wait}"),
                    None,
                    Some(&self.w1),
                )
                .await
                .ok()
                .clone();
            if w["state"] != "open" {
                return w;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("run {wait} never ended");
    }

    async fn grant(&self, secret: &str, body: Value) {
        let r = self
            .td
            .post(&format!("/api/secrets/p/{secret}/grants"), body)
            .await;
        assert!(r.status.is_success(), "{}", r.body);
    }
}

fn clean(what: &str, text: &str) {
    for v in [DEPLOY, WEAK, &DEPLOY[..12]] {
        assert!(!text.contains(v), "{what} holds `{v}`: {text}");
    }
}

/// A script that prints `$var` in two writes (two output chunks), to
/// stdout and stderr, and its length, which proves the process got it.
fn print_env(var: &str) -> String {
    format!(
        "v=\"${var}\"; n=${{#v}}; h=$((n / 2)); \
         a=$(printf '%s' \"$v\" | cut -c1-$h); b=$(printf '%s' \"$v\" | cut -c$((h + 1))-); \
         printf '{var}=%s' \"$a\"; sleep 0.1; printf '%s\\n' \"$b\"; \
         printf 'err %s\\n' \"$v\" >&2; echo \"len=$n\"; echo \"args=$*\""
    )
}

#[tokio::test]
async fn a_run_grant_injects_on_its_target_and_redacts_everywhere() {
    let lab = lab().await;
    let s = lab.script(
        "deploy.sh",
        &format!("{}\n{}", print_env("DEPLOY"), print_env("DB_PW")),
    );
    let env = json!({"DEPLOY": "@secret:deploy", "DB_PW": "@secret:db", "PLAIN": "1"});

    // No grant yet: 404, and nothing runs.
    let r = lab.run("box", &s, env.clone()).await;
    assert_eq!(r.status, 404, "{}", r.body);

    // A `run` grant on box.
    lab.grant(
        "deploy",
        json!({"grantee": "agent:p/w1", "level": "run", "targets": ["box"]}),
    )
    .await;
    lab.grant(
        "db",
        json!({"grantee": "agent:p/w1", "level": "run", "targets": ["box"]}),
    )
    .await;

    // Another target: 403 naming the allowed one, no value.
    let r = lab.run("web", &s, env.clone()).await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert!(r.body.to_string().contains("`box`"), "{}", r.body);
    clean("the refusal", &r.body.to_string());

    // box: the process gets both values; nobody else sees them.
    let r = lab.run("box", &s, env.clone()).await;
    assert_eq!(r.status, 201, "{}", r.body);
    clean("the start answer", &r.body.to_string());
    let wait = r.body["wait"]["id"].as_str().unwrap().to_owned();
    let session = r.body["session_id"].as_str().unwrap().to_owned();
    let w = lab.ended(&wait).await;
    let result = w["result"].to_string();
    clean("the wait result", &result);
    let out = w["result"]["stdout_tail"].as_str().unwrap_or_default();
    let err = w["result"]["stderr_tail"].as_str().unwrap_or_default();
    assert!(out.contains("DEPLOY=[secret:"), "{out}");
    assert!(out.contains(&format!("len={}", DEPLOY.len())), "{out}");
    assert!(out.contains("DB_PW=[secret:run/p/db]"), "{out}");
    assert!(out.contains(&format!("len={}", WEAK.len())), "{out}");
    assert!(err.contains("err [secret:"), "{err}");
    // The value is not on the command line.
    assert!(out.contains("args=\n"), "{out}");

    // The run log.
    let log = lab
        .td
        .req(
            hyper::Method::GET,
            &format!("/api/waits/{wait}/log"),
            None,
            Some(&lab.w1),
        )
        .await;
    let text = log.ok()["output"].as_str().unwrap().to_owned();
    clean("the run log", &text);
    assert!(text.contains("DB_PW=[secret:run/p/db]"), "{text}");

    // The audit: one `secret.run` row per secret, with the target, the
    // session, and the run. No row holds a value.
    let audit = lab.td.get("/api/audit?action=secret.run").await;
    let rows = audit.ok().as_array().unwrap().clone();
    assert_eq!(rows.len(), 2, "{rows:?}");
    for row in &rows {
        let d = &row["detail"];
        assert_eq!(d["target"], "box", "{row}");
        assert_eq!(d["run"], wait.as_str(), "{row}");
        assert_eq!(d["session"], session.as_str(), "{row}");
        assert_eq!(d["level"], "run", "{row}");
    }
    let names: Vec<&str> = rows
        .iter()
        .filter_map(|r| r["detail"]["env"].as_str())
        .collect();
    assert!(
        names.contains(&"DEPLOY") && names.contains(&"DB_PW"),
        "{names:?}"
    );
    let all = lab.td.get("/api/audit?limit=500").await;
    clean("the audit", &all.body.to_string());
    let exec = lab.td.get("/api/audit?action=target.exec").await;
    assert!(exec.body.to_string().contains("DEPLOY"), "{}", exec.body);
    clean("the target.exec row", &exec.body.to_string());
    // The events and the mail of the run end.
    let ev = lab.td.get("/api/events?limit=500").await;
    clean("the events", &ev.body.to_string());
    let mail = lab.td.mail_of("p/w1").await;
    clean("the mail", &serde_json::to_string(&mail).unwrap());

    // No `secret get`, and no env var at the start.
    let r = lab
        .td
        .req(
            hyper::Method::POST,
            "/api/agent/secrets/deploy/get",
            Some(json!({})),
            Some(&lab.w1),
        )
        .await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert!(r.body.to_string().contains("`run`"), "{}", r.body);
    clean("the get refusal", &r.body.to_string());
    let r = lab
        .td
        .req(
            hyper::Method::POST,
            "/api/agent/secrets/env",
            Some(json!({})),
            Some(&lab.w1),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["vars"], json!({}), "{}", r.body);
    // The agent's list shows the level and the targets, no value.
    let r = lab
        .td
        .req(
            hyper::Method::GET,
            "/api/agent/secrets",
            None,
            Some(&lab.w1),
        )
        .await;
    let deploy = r
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "deploy")
        .unwrap()
        .clone();
    assert_eq!(deploy["level"], "run");
    assert_eq!(deploy["targets"], json!(["box"]));
    clean("the agent's list", &r.body.to_string());
}

/// The grant is read at each run start: a grant that the operator adds
/// or removes while the agent works applies at its next run.
#[tokio::test]
async fn the_grant_is_checked_at_each_run() {
    let lab = lab().await;
    let s = lab.script("len.sh", "v=\"$DEPLOY\"; echo \"len=${#v}\"");
    let env = json!({"DEPLOY": "@secret:deploy"});
    lab.grant(
        "deploy",
        json!({"grantee": "agent:p/w1", "level": "run", "targets": ["box"]}),
    )
    .await;
    assert_eq!(lab.run("web", &s, env.clone()).await.status, 403);
    // web added: the next run works, with no restart.
    lab.grant(
        "deploy",
        json!({"grantee": "agent:p/w1", "level": "run", "targets": ["box", "web"]}),
    )
    .await;
    let r = lab.run("web", &s, env.clone()).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let w = lab.ended(r.body["wait"]["id"].as_str().unwrap()).await;
    let out = w["result"]["stdout_tail"].as_str().unwrap_or_default();
    assert!(out.contains(&format!("len={}", DEPLOY.len())), "{out}");
    // Revoked: the next run is refused.
    let r = lab
        .td
        .delete("/api/secrets/p/deploy/grants?grantee=agent:p/w1")
        .await;
    assert_eq!(r.status, 204, "{}", r.body);
    assert_eq!(lab.run("box", &s, env.clone()).await.status, 404);
    // Down to `use`: refused too.
    lab.grant("deploy", json!({"grantee": "agent:p/w1", "level": "use"}))
        .await;
    assert_eq!(lab.run("box", &s, env).await.status, 403);
}

/// The grant API: `run` needs targets that exist; the other levels take
/// none.
#[tokio::test]
async fn a_run_grant_names_existing_targets() {
    let lab = lab().await;
    let post = |body: Value| {
        let td = &lab.td;
        async move { td.post("/api/secrets/p/deploy/grants", body).await }
    };
    let r = post(json!({"grantee": "agent:p/w1", "level": "run"})).await;
    assert_eq!(r.status, 422, "{}", r.body);
    let r = post(json!({"grantee": "agent:p/w1", "level": "run", "targets": ["nope"]})).await;
    assert_eq!(r.status, 404, "{}", r.body);
    let r = post(json!({"grantee": "agent:p/w1", "level": "read", "targets": ["box"]})).await;
    assert_eq!(r.status, 422, "{}", r.body);
    let r = post(json!({"grantee": "agent:p/w1", "level": "run", "targets": ["box"], "env": "DEPLOY"}))
        .await;
    assert_eq!(r.status, 422, "{}", r.body);
    let r = post(json!({"grantee": "agent:p/w1", "level": "run", "targets": ["box"]})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let audit = lab.td.get("/api/audit?action=secret.grant").await;
    assert!(
        audit.body.to_string().contains("\"targets\":[\"box\"]"),
        "{}",
        audit.body
    );
}
