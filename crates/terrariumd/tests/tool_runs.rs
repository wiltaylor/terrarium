//! Tool proxies, the run path (GitHub #21, design D4 and D6, TER-30): a
//! real `terrarium_host::Client` runs the commands on this machine, and
//! the tests run tools through `POST /api/tools/{name}/run` as an agent.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use base64::Engine;
use common::TestDaemon;
use serde_json::{Value, json};
use terrarium_core::protocol::HostCapabilities;
use terrarium_host::{Client, ClientOptions};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
/// The value of the global secret `tt`, set as `TRACKER_TOKEN`.
const TOKEN: &str = "gV7q2LmZ9xTr4KwB8nPdQs5Hy";

struct Lab {
    td: TestDaemon,
    scripts: tempfile::TempDir,
    w1: String,
    w2: String,
}

/// A daemon with a host agent `lab1` online, the global host target `box`,
/// the secret `tt`, agents `p/w1` and `p/w2`, and the scripts dir.
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
    td.post(
        "/api/targets",
        json!({"name": "box", "kind": "host", "host": "lab1"}),
    )
    .await
    .ok();
    td.project("p").await;
    let w1 = td.agent("p", "w1", json!({})).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let w2 = td.agent("p", "w2", json!({})).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    td.put(
        "/api/secrets/global/tt",
        json!({"kind": "token", "fields": {"value": TOKEN}}),
    )
    .await
    .ok();
    Lab {
        td,
        scripts,
        w1,
        w2,
    }
}

impl Lab {
    /// A script in the scripts dir, mode 0755.
    fn script(&self, name: &str, body: &str) -> String {
        let p = self.scripts.path().join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.display().to_string()
    }

    /// Define tool `name` on `box` that runs `script`, with `extra`, and
    /// grant it to `p/w1`.
    async fn tool(&self, name: &str, script: &str, extra: Value) {
        let mut def = json!({"name": name, "target": "box", "command": [script]});
        for (k, v) in extra.as_object().unwrap() {
            def[k] = v.clone();
        }
        let r = self.td.post("/api/tools", def).await;
        assert_eq!(r.status, 201, "{}", r.body);
        let r = self
            .td
            .post(
                &format!("/api/tools/{name}/grants"),
                json!({"grantee": "agent:p/w1"}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
    }

    async fn run_as(&self, agent: &str, name: &str, argv: &[&str]) -> common::Resp {
        self.td
            .post_as(agent, &format!("/api/tools/{name}/run"), json!({"argv": argv}))
            .await
    }
}

#[derive(Debug, Default)]
struct Drained {
    stdout: String,
    stderr: String,
    end: Value,
    result: Value,
}

/// Read session `sid` on `box` to its end, as `agent`.
async fn drain(td: &TestDaemon, agent: &str, sid: &str) -> Drained {
    let mut d = Drained::default();
    let mut since = 0u64;
    for _ in 0..300 {
        let path = format!("/api/targets/box/sessions/{sid}/output?since={since}&wait=5");
        let r = td.req(hyper::Method::GET, &path, None, Some(agent)).await;
        let o = r.ok().clone();
        for c in o["chunks"].as_array().unwrap() {
            let bytes = B64.decode(c["data"].as_str().unwrap()).unwrap();
            let text = String::from_utf8_lossy(&bytes).into_owned();
            match c["stream"].as_str().unwrap() {
                "stderr" => d.stderr.push_str(&text),
                _ => d.stdout.push_str(&text),
            }
        }
        since = o["next"].as_u64().unwrap();
        if o["done"] == true {
            d.end = o["end"].clone();
            d.result = o["result"].clone();
            return d;
        }
    }
    panic!("session {sid} never ended");
}

fn sid(r: &common::Resp) -> String {
    assert_eq!(r.status, 201, "{}", r.body);
    r.body["session_id"].as_str().unwrap().to_owned()
}

/// The agent's arguments arrive as separate argv entries, with no shell:
/// `$(...)`, `;`, and spaces stay literal. The run writes a `tool.run`
/// audit row and two `tool.run` events.
#[tokio::test]
async fn the_arguments_go_as_argv_with_no_shell_and_the_run_is_audited() {
    let lab = lab().await;
    let s = lab.script("args.sh", r#"for a in "$@"; do printf '[%s]\n' "$a"; done"#);
    lab.tool("tracker-read", &s, json!({"args": {"allow": ["show"]}}))
        .await;
    let r = lab
        .run_as(&lab.w1, "tracker-read", &["show", "$(id)", "a;b", "x y", "`ls`"])
        .await;
    assert_eq!(r.body["tool"], "tracker-read");
    assert_eq!(r.body["target"], "box");
    let d = drain(&lab.td, &lab.w1, &sid(&r)).await;
    assert_eq!(d.stdout, "[show]\n[$(id)]\n[a;b]\n[x y]\n[`ls`]\n");
    assert_eq!(d.end["type"], "exit", "{:?}", d.end);
    assert_eq!(d.end["code"], 0);
    assert_eq!(d.result["truncated"], false);

    let mut rows = Vec::new();
    for _ in 0..100 {
        rows = lab.td.get("/api/audit?action=tool.run").await.ok().as_array().unwrap().clone();
        if !rows.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(rows.len(), 1, "{rows:#?}");
    let detail = &rows[0]["detail"];
    assert_eq!(detail["tool"], "tracker-read");
    assert_eq!(detail["target"], "box");
    assert_eq!(detail["agent"], "p/w1");
    assert_eq!(detail["argv"][1], "$(id)");
    assert_eq!(detail["exit"], 0);
    assert_eq!(detail["truncated"], false);
    let ev = lab.td.get("/api/events?kind=tool.run&limit=10").await.ok().clone();
    let changes: Vec<&str> = ev
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["data"]["change"].as_str().unwrap())
        .collect();
    assert!(changes.contains(&"started") && changes.contains(&"ended"), "{ev:#?}");
}

/// The body carries the arguments only; each argument rule refuses.
#[tokio::test]
async fn the_body_and_the_argument_rules_refuse() {
    let lab = lab().await;
    let s = lab.script("args.sh", r#"echo "$@""#);
    lab.tool(
        "tracker-read",
        &s,
        json!({"args": {"allow": ["show"], "deny_flags": ["--config", "-o"], "max": 3}}),
    )
    .await;
    for extra in [
        json!({"cwd": "/"}),
        json!({"env": {"X": "1"}}),
        json!({"target": "other"}),
        json!({"shell": true}),
    ] {
        let mut body = json!({"argv": ["show"]});
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        let r = lab
            .td
            .post_as(&lab.w1, "/api/tools/tracker-read/run", body.clone())
            .await;
        assert!(r.status == 400 || r.status == 422, "{body}: {} {}", r.status, r.body);
    }
    for argv in [
        vec!["delete"],
        vec![],
        vec!["show", "--conf=/etc/x"],
        vec!["show", "-xo/tmp/out"],
        vec!["show", "a", "b", "c"],
    ] {
        lab.run_as(&lab.w1, "tracker-read", &argv)
            .await
            .err(422, "invalid");
    }
}

/// A script that prints its token in two writes gets it redacted: the run's
/// own secret, split across chunks.
#[tokio::test]
async fn the_output_is_redacted_across_chunks() {
    let lab = lab().await;
    let s = lab.script(
        "leak.sh",
        r#"printf 'token=%s' "$(printf '%s' "$TRACKER_TOKEN" | cut -c1-9)"; sleep 0.3
printf '%s\n' "$(printf '%s' "$TRACKER_TOKEN" | cut -c10-)"
printf 'err %s\n' "$TRACKER_TOKEN" >&2"#,
    );
    lab.tool("leaky", &s, json!({"env_secrets": {"TRACKER_TOKEN": "tt"}}))
        .await;
    let r = lab.run_as(&lab.w1, "leaky", &[]).await;
    let d = drain(&lab.td, &lab.w1, &sid(&r)).await;
    assert!(!d.stdout.contains(TOKEN) && !d.stderr.contains(TOKEN), "{d:?}");
    assert!(!d.stdout.contains(&TOKEN[..9]), "{d:?}");
    assert_eq!(d.stdout, "token=[secret:tool/tt]\n", "{d:?}");
    assert_eq!(d.stderr, "err [secret:tool/tt]\n", "{d:?}");
}

/// Past `max_output` the run is killed and ends `truncated`.
#[tokio::test]
async fn the_output_cap_kills_the_run() {
    let lab = lab().await;
    let s = lab.script("flood.sh", "while :; do echo xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done");
    lab.tool("flood", &s, json!({"max_output": "2K", "timeout": "30s"}))
        .await;
    let r = lab.run_as(&lab.w1, "flood", &[]).await;
    let d = drain(&lab.td, &lab.w1, &sid(&r)).await;
    assert!(d.stdout.len() <= 2048, "{}", d.stdout.len());
    assert!(d.stdout.len() > 1000, "{}", d.stdout.len());
    assert_eq!(d.end["type"], "error", "{:?}", d.end);
    assert_eq!(d.end["code"], "truncated");
    assert_eq!(d.result["truncated"], true);
}

/// The rate of a tool, per agent.
#[tokio::test]
async fn the_rate_limits_the_runs() {
    let lab = lab().await;
    let s = lab.script("ok.sh", "echo ok");
    lab.tool("quick", &s, json!({"rate": "2/m"})).await;
    for _ in 0..2 {
        let r = lab.run_as(&lab.w1, "quick", &[]).await;
        drain(&lab.td, &lab.w1, &sid(&r)).await;
    }
    let r = lab.run_as(&lab.w1, "quick", &[]).await;
    r.err(429, "rate_limited");
    assert!(r.body["error"]["details"]["retry_after"].as_u64().unwrap() >= 1);
}

/// Only an agent with a grant runs the tool, and only the caller reads its
/// session. A revoke while the agent runs stops the next run.
#[tokio::test]
async fn a_grant_decides_and_a_revoke_applies_at_once() {
    let lab = lab().await;
    let s = lab.script("ok.sh", "echo ok");
    lab.tool("quick", &s, json!({})).await;
    let w2 = lab.w2.clone();
    lab.run_as(&w2, "quick", &[]).await.err(404, "not_found");
    lab.run_as(&lab.w1, "nope", &[]).await.err(404, "not_found");
    let r = lab.run_as(&lab.w1, "quick", &[]).await;
    let session = sid(&r);
    // Another agent cannot read the session.
    let path = format!("/api/targets/box/sessions/{session}/output?since=0");
    let other = lab.td.req(hyper::Method::GET, &path, None, Some(&w2)).await;
    other.err(404, "not_found");
    assert_eq!(drain(&lab.td, &lab.w1, &session).await.stdout, "ok\n");
    // The operator may run it (a test run).
    let r = lab.td.post("/api/tools/quick/run", json!({"argv": []})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    // A project grant covers w2 too.
    lab.td
        .post("/api/tools/quick/grants", json!({"grantee": "project:p"}))
        .await
        .ok();
    let r = lab.run_as(&w2, "quick", &[]).await;
    assert_eq!(drain(&lab.td, &w2, &sid(&r)).await.stdout, "ok\n");
    // The revoke: the next run is refused.
    lab.td
        .delete("/api/tools/quick/grants?grantee=agent:p/w1")
        .await
        .ok();
    lab.td
        .delete("/api/tools/quick/grants?grantee=project:p")
        .await
        .ok();
    lab.run_as(&lab.w1, "quick", &[]).await.err(404, "not_found");
    lab.run_as(&w2, "quick", &[]).await.err(404, "not_found");
}

/// A tool without `stdin` gets none, whichever route sends it; a tool with
/// `stdin` gets it.
#[tokio::test]
async fn stdin_reaches_only_a_tool_that_allows_it() {
    let lab = lab().await;
    let s = lab.script("read.sh", r#"x=; read -r x; echo "got:$x""#);
    lab.tool("closed", &s, json!({"timeout": "5s"})).await;
    lab.tool("open", &s, json!({"stdin": true, "timeout": "5s"})).await;
    for (tool, want) in [("closed", "got:\n"), ("open", "got:hello\n")] {
        let r = lab.run_as(&lab.w1, tool, &[]).await;
        let session = sid(&r);
        let input = format!("/api/targets/box/sessions/{session}/input");
        lab.td
            .post_as(&lab.w1, &input, json!({"data": B64.encode("hello\n"), "eof": true}))
            .await;
        let d = drain(&lab.td, &lab.w1, &session).await;
        // The closed tool: stdin is closed at the start, so its read
        // ends at once with nothing.
        assert_eq!(d.stdout, want, "{tool}: {d:?}");
    }
}
