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
/// The value of the global generic secret `pin`: shorter than 8 bytes.
const PIN: &str = "q7Z2p9K";

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
    // A short value that the daemon matcher leaves out (under 8 bytes).
    td.post(
        "/api/secrets",
        json!({"name": "pin", "kind": "generic", "fields": {"pin": PIN},
               "secret_fields": ["pin"]}),
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
            .post_as(
                agent,
                &format!("/api/tools/{name}/run"),
                json!({"argv": argv}),
            )
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
        let path = format!("/api/tools/sessions/{sid}/output?since={since}&wait=5");
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
        .run_as(
            &lab.w1,
            "tracker-read",
            &["show", "$(id)", "a;b", "x y", "`ls`"],
        )
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
        rows = lab
            .td
            .get("/api/audit?action=tool.run")
            .await
            .ok()
            .as_array()
            .unwrap()
            .clone();
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
    let ev = lab
        .td
        .get("/api/events?kind=tool.run&limit=10")
        .await
        .ok()
        .clone();
    let changes: Vec<&str> = ev
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["data"]["change"].as_str().unwrap())
        .collect();
    assert!(
        changes.contains(&"started") && changes.contains(&"ended"),
        "{ev:#?}"
    );
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
        assert!(
            r.status == 400 || r.status == 422,
            "{body}: {} {}",
            r.status,
            r.body
        );
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
    assert!(
        !d.stdout.contains(TOKEN) && !d.stderr.contains(TOKEN),
        "{d:?}"
    );
    assert!(!d.stdout.contains(&TOKEN[..9]), "{d:?}");
    assert_eq!(d.stdout, "token=[secret:tool/tt]\n", "{d:?}");
    assert_eq!(d.stderr, "err [secret:tool/tt]\n", "{d:?}");
}

/// Past `max_output` the run is killed and ends `truncated`.
#[tokio::test]
async fn the_output_cap_kills_the_run() {
    let lab = lab().await;
    let s = lab.script(
        "flood.sh",
        "while :; do echo xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done",
    );
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
    let path = format!("/api/tools/sessions/{session}/output?since=0");
    let other = lab.td.req(hyper::Method::GET, &path, None, Some(&w2)).await;
    other.err(404, "not_found");
    // The target session routes do not show a tool session, also not to
    // its own agent.
    let target_path = format!("/api/targets/box/sessions/{session}/output?since=0");
    let r = lab
        .td
        .req(hyper::Method::GET, &target_path, None, Some(&lab.w1))
        .await;
    r.err(404, "not_found");
    assert_eq!(drain(&lab.td, &lab.w1, &session).await.stdout, "ok\n");
    // The operator may run it (a test run).
    let r = lab
        .td
        .post("/api/tools/quick/run", json!({"argv": []}))
        .await;
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
    lab.run_as(&lab.w1, "quick", &[])
        .await
        .err(404, "not_found");
    lab.run_as(&w2, "quick", &[]).await.err(404, "not_found");
}

/// A tool without `stdin` gets none, whichever route sends it; a tool with
/// `stdin` gets it.
#[tokio::test]
async fn stdin_reaches_only_a_tool_that_allows_it() {
    let lab = lab().await;
    let s = lab.script("read.sh", r#"x=; read -r x; echo "got:$x""#);
    lab.tool("tt-no-stdin", &s, json!({"timeout": "5s"})).await;
    lab.tool("tt-with-stdin", &s, json!({"stdin": true, "timeout": "5s"}))
        .await;
    for (tool, want) in [("tt-no-stdin", "got:\n"), ("tt-with-stdin", "got:hello\n")] {
        let r = lab.run_as(&lab.w1, tool, &[]).await;
        let session = sid(&r);
        let input = format!("/api/tools/sessions/{session}/input");
        let r = lab
            .td
            .post_as(
                &lab.w1,
                &input,
                json!({"data": B64.encode("hello\n"), "eof": true}),
            )
            .await;
        if tool == "tt-no-stdin" {
            r.err(422, "invalid");
        } else {
            assert_eq!(r.status, 200, "{}", r.body);
        }
        let d = drain(&lab.td, &lab.w1, &session).await;
        // The closed tool: stdin is closed at the start, so its read
        // ends at once with nothing.
        assert_eq!(d.stdout, want, "{tool}: {d:?}");
    }
}

/// The run's own secret goes also when it is shorter than 8 bytes, which
/// the daemon matcher leaves out (lead's F3).
#[tokio::test]
async fn a_short_env_secret_is_redacted_too() {
    let lab = lab().await;
    let s = lab.script("pin.sh", r#"printf 'pin=%s\n' "$DOOR_PIN""#);
    lab.tool("door", &s, json!({"env_secrets": {"DOOR_PIN": "pin"}}))
        .await;
    let r = lab.run_as(&lab.w1, "door", &[]).await;
    let d = drain(&lab.td, &lab.w1, &sid(&r)).await;
    assert!(!d.stdout.contains(PIN), "{d:?}");
    assert_eq!(d.stdout, "pin=[secret:tool/pin]\n", "{d:?}");
}

/// DELETE kills the run; then the session is gone.
#[tokio::test]
async fn a_delete_kills_the_run() {
    let lab = lab().await;
    let s = lab.script("slow.sh", "echo start; sleep 30; echo late");
    lab.tool("slow", &s, json!({"timeout": "60s"})).await;
    let r = lab.run_as(&lab.w1, "slow", &[]).await;
    let session = sid(&r);
    let path = format!("/api/tools/sessions/{session}");
    let other = lab
        .td
        .req(hyper::Method::DELETE, &path, None, Some(&lab.w2))
        .await;
    other.err(404, "not_found");
    let r = lab
        .td
        .req(hyper::Method::DELETE, &path, None, Some(&lab.w1))
        .await;
    assert_eq!(r.status, 204, "{}", r.body);
    let out = format!("/api/tools/sessions/{session}/output?since=0");
    lab.td
        .req(hyper::Method::GET, &out, None, Some(&lab.w1))
        .await
        .err(404, "not_found");
}

/// The `terrarium` CLI binary next to the test binaries. A missing one is
/// built once (as tests/runner_process.rs does).
fn cli_bin() -> std::path::PathBuf {
    static BIN: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    BIN.get_or_init(|| {
        let exe = std::env::current_exe().unwrap();
        let dir = exe.parent().and_then(std::path::Path::parent).unwrap();
        let bin = dir.join("terrarium");
        if !bin.is_file() {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let status = std::process::Command::new(cargo)
                .args(["build", "-q", "-p", "terrarium", "--bin", "terrarium"])
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .status()
                .unwrap();
            assert!(status.success(), "cargo build -p terrarium failed");
        }
        assert!(bin.is_file(), "no CLI binary at {}", bin.display());
        bin
    })
    .clone()
}

/// Run the tool link `link` (a symlink to the CLI named as the tool) as
/// the operator over the daemon's socket, with `args` and `stdin`. The
/// stdout, the stderr, and the exit code.
async fn run_link(
    td: &TestDaemon,
    link: &std::path::Path,
    args: &[&str],
    stdin: &str,
) -> (String, String, i32) {
    use tokio::io::AsyncWriteExt;
    let mut c = tokio::process::Command::new(link);
    c.args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env(terrarium_core::paths::ENV_SOCKET, &td.socket)
        .env("XDG_CONFIG_HOME", link.parent().unwrap().join("xdg"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = c.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    input.write_all(stdin.as_bytes()).await.unwrap();
    drop(input);
    let out = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .expect("the tool link did not end")
        .unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

/// End to end through a tool link (design D3, TER-34): a symlink named as
/// the tool runs `terrarium tool run <tool> -- <args>`. The arguments
/// arrive as they are, stdout and stderr come back apart, and the CLI exits
/// with the tool's exit code. Stdin reaches only a tool with `stdin`.
#[tokio::test]
async fn a_tool_link_runs_the_tool_and_exits_with_its_code() {
    let lab = lab().await;
    let echo = lab.script(
        "echo-args.sh",
        r#"echo "args:$*"; echo "to stderr" >&2; exit 3"#,
    );
    lab.tool("tt-link-echo", &echo, json!({"timeout": "10s"}))
        .await;
    let read = lab.script("read-link.sh", r#"x=; read -r x; echo "got:$x""#);
    lab.tool("tt-link-closed", &read, json!({"timeout": "10s"}))
        .await;
    lab.tool(
        "tt-link-open",
        &read,
        json!({"stdin": true, "timeout": "10s"}),
    )
    .await;
    let links = tempfile::tempdir().unwrap();
    let link = |name: &str| {
        let p = links.path().join(name);
        std::os::unix::fs::symlink(cli_bin(), &p).unwrap();
        p
    };
    let (out, err, code) = run_link(
        &lab.td,
        &link("tt-link-echo"),
        &["show", "$(id)", "a b"],
        "",
    )
    .await;
    assert_eq!(out, "args:show $(id) a b\n", "{err}");
    assert!(err.contains("to stderr"), "{err}");
    assert_eq!(code, 3, "{out} {err}");
    // A tool without stdin: the CLI sends none, and the tool reads nothing.
    let (out, err, code) = run_link(&lab.td, &link("tt-link-closed"), &[], "hello\n").await;
    assert_eq!((out.as_str(), code), ("got:\n", 0), "{err}");
    // With stdin: the line arrives.
    let (out, err, code) = run_link(&lab.td, &link("tt-link-open"), &[], "hello\n").await;
    assert_eq!((out.as_str(), code), ("got:hello\n", 0), "{err}");
    // An unknown tool: the daemon's message, exit 1.
    let (_, err, code) = run_link(&lab.td, &link("tt-link-none"), &[], "").await;
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("not_found"), "{err}");
}

/// A detached run (`terrarium tool run --detach`, TER-32): the agent puts a
/// `process` wait on its tool session. The wait resolves with the tool's
/// exit code and the redacted output tail, and the run log is redacted
/// too. Another agent cannot wait on the session.
#[tokio::test]
async fn a_detached_tool_run_ends_in_a_process_wait() {
    let lab = lab().await;
    let s = lab.script(
        "detached.sh",
        r#"echo "token=$TRACKER_TOKEN"; echo "on stderr" >&2; exit 4"#,
    );
    lab.tool(
        "tt-detached",
        &s,
        json!({"env_secrets": {"TRACKER_TOKEN": "tt"}, "timeout": "10s"}),
    )
    .await;
    let r = lab.run_as(&lab.w1, "tt-detached", &[]).await;
    let session = sid(&r);
    let waits = format!("/api/agents/{}/waits", lab.w1);
    lab.td
        .post_as(
            &lab.w2,
            &format!("/api/agents/{}/waits", lab.w2),
            json!({"kind": "process", "session": session}),
        )
        .await
        .err(404, "not_found");
    let w = lab
        .td
        .post_as(
            &lab.w1,
            &waits,
            json!({"kind": "process", "session": session, "reason": "tool tt-detached"}),
        )
        .await;
    assert_eq!(w.status, 201, "{}", w.body);
    let summary = w.body["summary"].as_str().unwrap().to_owned();
    assert!(
        summary.starts_with("tool run ") && summary.ends_with(" on box"),
        "{summary}"
    );
    let wid = w.body["id"].as_str().unwrap().to_owned();
    let done = loop {
        let v = lab
            .td
            .get_as(&lab.w1, &format!("/api/waits/{wid}"))
            .await
            .ok()
            .clone();
        if v["state"] == "resolved" {
            break v;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(done["result"]["exit_code"], 4, "{done}");
    let tail = done["result"]["stdout_tail"].as_str().unwrap();
    assert!(!tail.contains(TOKEN), "{tail}");
    assert!(tail.starts_with("token=[secret:"), "{tail}");
    assert_eq!(done["result"]["stderr_tail"], "on stderr\n");
    let log = lab
        .td
        .get_as(&lab.w1, &format!("/api/waits/{wid}/log"))
        .await
        .ok()
        .clone();
    assert!(!log.to_string().contains(TOKEN), "{log}");

/// TER-31 (design D5): a tool with `approval = true` answers an agent's run
/// with `202 pending` and a `tool_run` question in the Inbox. Only the
/// operator answers it; the agent gets no mail. The run with the approval
/// starts once; another argv or a second use is `409`.
#[tokio::test]
async fn an_approved_run_starts_once() {
    let lab = lab().await;
    lab.td.project("ops").await;
    let sys = lab
        .td
        .agent("ops", "root", json!({"roles": ["sysadmin"]}))
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let s = lab.script("ok.sh", r#"echo "ran:$*""#);
    lab.tool("tt-approve", &s, json!({"approval": true})).await;
    let r = lab.run_as(&lab.w1, "tt-approve", &["show", "T-1"]).await;
    assert_eq!(r.status, 202, "{}", r.body);
    assert_eq!(r.body["pending"], true);
    assert!(r.body["expires_at"].is_string());
    let q = r.body["question"].as_str().unwrap().to_owned();
    // The same run again: the same question, not a second one.
    let again = lab.run_as(&lab.w1, "tt-approve", &["show", "T-1"]).await;
    assert_eq!(again.body["question"], q.as_str());

    let question = lab
        .td
        .get(&format!("/api/questions/{q}"))
        .await
        .ok()
        .clone();
    assert_eq!(question["kind"], "tool_run");
    assert_eq!(question["options"], json!(["Run", "Refuse"]));
    assert_eq!(question["tool_run"]["argv"], json!(["show", "T-1"]));
    assert!(question["context"].as_str().unwrap().contains("\"T-1\""));
    let inbox = lab.td.get("/api/inbox?kind=question").await.ok().clone();
    assert!(
        inbox
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["id"] == format!("tool_run:{q}")),
        "{inbox:#?}"
    );
    // The agent's own list leaves it out; the agent may still read it.
    let own = lab
        .td
        .get_as(&lab.w1, &format!("/api/agents/{}/questions", lab.w1))
        .await;
    if own.status == 200 {
        assert!(
            own.body
                .as_array()
                .unwrap()
                .iter()
                .all(|x| x["id"] != q.as_str())
        );
    }
    lab.td
        .get_as(&lab.w1, &format!("/api/questions/{q}"))
        .await
        .ok();

    // Not answered yet: the approval does not run.
    let early = lab
        .td
        .post_as(
            &lab.w1,
            "/api/tools/tt-approve/run",
            json!({"argv": ["show", "T-1"], "approval": q}),
        )
        .await;
    early.err(409, "conflict");
    // A sysadmin agent cannot approve another agent's run.
    lab.td
        .post_as(
            &sys,
            &format!("/api/questions/{q}/answer"),
            json!({"option": 0}),
        )
        .await
        .err(403, "forbidden");
    lab.td
        .post(&format!("/api/questions/{q}/answer"), json!({"option": 0}))
        .await
        .ok();
    // Another argv with the approval: refused.
    lab.td
        .post_as(
            &lab.w1,
            "/api/tools/tt-approve/run",
            json!({"argv": ["show", "T-2"], "approval": q}),
        )
        .await
        .err(409, "conflict");
    let ok = lab
        .td
        .post_as(
            &lab.w1,
            "/api/tools/tt-approve/run",
            json!({"argv": ["show", "T-1"], "approval": q}),
        )
        .await;
    let d = drain(&lab.td, &lab.w1, &sid(&ok)).await;
    assert_eq!(d.stdout, "ran:show T-1\n");
    // A second use: refused.
    let reuse = lab
        .td
        .post_as(
            &lab.w1,
            "/api/tools/tt-approve/run",
            json!({"argv": ["show", "T-1"], "approval": q}),
        )
        .await;
    reuse.err(409, "conflict");
    assert!(
        reuse.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("used already")
    );
    // The audit row names the approval and who gave it.
    let mut rows = Vec::new();
    for _ in 0..100 {
        rows = lab
            .td
            .get("/api/audit?action=tool.run")
            .await
            .ok()
            .as_array()
            .unwrap()
            .clone();
        if !rows.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(rows[0]["detail"]["approval"], q.as_str());
    assert_eq!(rows[0]["detail"]["approved_by"], "operator");
    // No mail to the agent: its CLI waits on the question.
    let mail = lab
        .td
        .get(&format!("/api/mail?to={}", lab.w1))
        .await
        .ok()
        .clone();
    assert!(mail.as_array().unwrap().is_empty(), "{mail:#?}");
    // The operator's own test run needs no approval.
    let r = lab
        .td
        .post("/api/tools/tt-approve/run", json!({"argv": ["x"]}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
}

/// `approval.first`: only a listed first argument asks. A refuse makes the
/// approval useless; the agent may retract its own approval question.
#[tokio::test]
async fn approval_first_and_refuse() {
    let lab = lab().await;
    let s = lab.script("ok.sh", r#"echo "ran:$*""#);
    lab.tool(
        "tt-first",
        &s,
        json!({"approval": {"first": ["close"]}, "args": {"allow": ["show", "close"]}}),
    )
    .await;
    let r = lab.run_as(&lab.w1, "tt-first", &["show", "T-1"]).await;
    assert_eq!(
        drain(&lab.td, &lab.w1, &sid(&r)).await.stdout,
        "ran:show T-1\n"
    );
    let p = lab.run_as(&lab.w1, "tt-first", &["close", "T-1"]).await;
    assert_eq!(p.status, 202, "{}", p.body);
    let q = p.body["question"].as_str().unwrap().to_owned();
    lab.td
        .post(
            &format!("/api/questions/{q}/answer"),
            json!({"option": "Refuse"}),
        )
        .await
        .ok();
    let r = lab
        .td
        .post_as(
            &lab.w1,
            "/api/tools/tt-first/run",
            json!({"argv": ["close", "T-1"], "approval": q}),
        )
        .await;
    r.err(409, "conflict");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("refused")
    );
    // A new pending run; the member agent gives it up itself.
    let p = lab.run_as(&lab.w1, "tt-first", &["close", "T-9"]).await;
    let q = p.body["question"].as_str().unwrap().to_owned();
    let r = lab
        .td
        .post_as(&lab.w1, &format!("/api/questions/{q}/retract"), json!({}))
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
}

/// The approval timeout: the sweep retracts the question.
#[tokio::test]
async fn an_approval_times_out() {
    let lab = lab().await;
    lab.td
        .daemon
        .as_ref()
        .unwrap()
        .state
        .tool_rates
        .set_sweep_every(Duration::from_millis(100));
    let s = lab.script("ok.sh", "echo ran");
    lab.tool(
        "tt-timeout",
        &s,
        json!({"approval": true, "approval_timeout": "1s"}),
    )
    .await;
    let p = lab.run_as(&lab.w1, "tt-timeout", &[]).await;
    let q = p.body["question"].as_str().unwrap().to_owned();
    let mut status = Value::Null;
    for _ in 0..200 {
        let v = lab
            .td
            .get(&format!("/api/questions/{q}"))
            .await
            .ok()
            .clone();
        status = v["status"].clone();
        if status == "retracted" {
            assert_eq!(v["retract_reason"], "the approval timed out");
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(status, "retracted");
}
