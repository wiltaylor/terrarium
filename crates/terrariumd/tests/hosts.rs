//! Host agents and targets: a real `terrarium_host::Client` dials the
//! daemon under test (docs/host-protocol.md), and the tests drive targets
//! through the API (docs/api.md §17).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use common::{TestDaemon, write};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use terrarium_core::protocol::{HostCapabilities, ToolCapability};
use terrarium_host::{Client, ClientOptions};
use terrarium_runner::{RequestContext, RequestHandler};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

struct Lab {
    td: TestDaemon,
    addr: SocketAddr,
    bins: tempfile::TempDir,
}

impl Lab {
    async fn start() -> Self {
        Self::with_config(|_| {}).await
    }

    async fn with_config(f: impl FnOnce(&Path)) -> Self {
        let td = TestDaemon::with_config(f).await;
        let state = td.daemon.as_ref().unwrap().state.clone();
        let (_task, addr) = terrariumd::hosts::serve(state, "127.0.0.1:0")
            .await
            .unwrap();
        let bins = tempfile::tempdir().unwrap();
        for tool in ["docker", "vmlab"] {
            let p = bins.path().join(tool);
            std::fs::write(&p, fake_tool(tool, bins.path())).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self { td, addr, bins }
    }

    fn state(&self) -> terrariumd::api::Shared {
        self.td.daemon.as_ref().unwrap().state.clone()
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.bins.path().join("calls.log")).unwrap_or_default()
    }

    async fn register(&self, name: &str) -> String {
        let r = self.td.post("/api/hosts", json!({"name": name})).await;
        assert_eq!(r.status, 201, "{}", r.body);
        r.body["token"].as_str().unwrap().to_owned()
    }

    fn client(&self, name: &str, token: &str) -> Client {
        let mut o = ClientOptions::new(format!("ws://{}/hosts/connect", self.addr), token, name);
        o.capabilities = HostCapabilities {
            docker: ToolCapability {
                present: true,
                version: Some("29.8.0".into()),
            },
            vmlab: ToolCapability {
                present: true,
                version: Some("0.30.0".into()),
            },
            shell: Some("/bin/sh".into()),
        };
        o.backoff_min = Duration::from_millis(50);
        o.backoff_max = Duration::from_millis(200);
        o.docker_bin = self.bins.path().join("docker").display().to_string();
        o.vmlab_bin = self.bins.path().join("vmlab").display().to_string();
        Client::new(o)
    }

    /// Register `name`, run its host agent, and wait for it to be online.
    async fn host(&self, name: &str) -> tokio::task::JoinHandle<()> {
        let token = self.register(name).await;
        let task = tokio::spawn(self.client(name, &token).run());
        self.wait_online(name, true).await;
        task
    }

    async fn wait_online(&self, name: &str, online: bool) {
        for _ in 0..300 {
            let r = self.td.get(&format!("/api/hosts/{name}")).await;
            if r.body["online"] == online {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("host {name} never became online={online}");
    }

    async fn target(&self, def: Value) {
        let r = self.td.post("/api/targets", def).await;
        assert_eq!(r.status, 201, "{}", r.body);
    }
}

/// A fake `docker` or `vmlab`: it records each call and answers like the
/// real tool for the verbs the daemon sends.
fn fake_tool(tool: &str, dir: &Path) -> String {
    let log = dir.join("calls.log");
    let state = dir.join(format!("{tool}.state"));
    let body = match tool {
        "docker" => format!(
            r#"case "$1" in
  inspect) if [ -f '{s}' ]; then echo '[{{"Id":"abc","Name":"/terrarium-dbox","State":{{"Status":"running","Running":true}}}}]'; else echo '[]'; echo 'Error: No such object' >&2; exit 1; fi ;;
  run) touch '{s}'; echo abc123 ;;
  rm) rm -f '{s}' ;;
esac"#,
            s = state.display()
        ),
        _ => format!(
            r#"case "$1" in
  up) touch '{s}' ;;
  down|destroy) rm -f '{s}' ;;
  status) printf 'lab "win"\n\n  NAME     KIND      STATUS   IP           TEMPLATE/IMAGE\n'
          if [ -f '{s}' ]; then st=running; else st=stopped; fi
          printf '  win11    vm        %s  10.0.0.9     x86_64/win11\n' "$st" ;;
esac"#,
            s = state.display()
        ),
    };
    format!(
        "#!/bin/sh\necho \"{tool} $* (in $(pwd))\" >> '{log}'\n{body}\nexit 0\n",
        log = log.display()
    )
}

#[derive(Debug, Default)]
struct Drained {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    file: Vec<u8>,
    end: Value,
}

async fn drain_as(td: &TestDaemon, agent: Option<&str>, target: &str, sid: &str) -> Drained {
    let mut d = Drained::default();
    let mut since = 0u64;
    for _ in 0..200 {
        let path = format!("/api/targets/{target}/sessions/{sid}/output?since={since}&wait=5");
        let r = td.req(hyper::Method::GET, &path, None, agent).await;
        let o = r.ok();
        for c in o["chunks"].as_array().unwrap() {
            let bytes = B64.decode(c["data"].as_str().unwrap()).unwrap();
            match c["stream"].as_str().unwrap() {
                "stdout" => d.stdout.extend(bytes),
                "stderr" => d.stderr.extend(bytes),
                _ => d.file.extend(bytes),
            }
        }
        since = o["next"].as_u64().unwrap();
        if o["done"] == true {
            d.end = o["end"].clone();
            return d;
        }
    }
    panic!("session {sid} never ended");
}

async fn drain(td: &TestDaemon, target: &str, sid: &str) -> Drained {
    drain_as(td, None, target, sid).await
}

async fn exec(td: &TestDaemon, target: &str, body: Value) -> String {
    let r = td.post(&format!("/api/targets/{target}/exec"), body).await;
    assert_eq!(r.status, 201, "{}", r.body);
    r.body["session_id"].as_str().unwrap().to_owned()
}

async fn input(td: &TestDaemon, target: &str, sid: &str, body: Value) {
    td.post(&format!("/api/targets/{target}/sessions/{sid}/input"), body)
        .await
        .ok();
}

async fn events(td: &TestDaemon, kind: &str) -> Vec<Value> {
    td.get(&format!("/api/events?kind={kind}"))
        .await
        .ok()
        .as_array()
        .unwrap()
        .clone()
}

#[tokio::test]
async fn hello_welcome_and_capabilities() {
    let lab = Lab::start().await;
    assert_eq!(lab.td.get("/api/hosts").await.body, json!([]));
    let token = lab.register("lab1").await;
    assert!(token.starts_with("thk_"));
    // The token is shown once.
    let listed = lab.td.get("/api/hosts").await;
    assert!(listed.ok()[0].get("token").is_none());
    assert_eq!(listed.body[0]["online"], false);
    lab.td
        .post("/api/hosts", json!({"name": "lab1"}))
        .await
        .err(409, "conflict");

    let task = tokio::spawn(lab.client("lab1", &token).run());
    lab.wait_online("lab1", true).await;
    let h = lab.td.get("/api/hosts/lab1").await;
    let h = h.ok();
    assert_eq!(h["status"], "online");
    assert_eq!(h["capabilities"]["docker"]["present"], true);
    assert_eq!(h["capabilities"]["vmlab"]["version"], "0.30.0");
    assert_eq!(h["summary"]["docker"], true);
    assert_eq!(h["os"], std::env::consts::OS);
    assert!(h["host_id"].is_string());
    let ev = events(&lab.td, "host.connected").await;
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0]["data"]["host"], "lab1");
    task.abort();
}

#[tokio::test]
async fn exec_streams_output_and_the_exit_code() {
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    lab.target(json!({"name": "box", "kind": "host", "host": "lab1"}))
        .await;
    let t = lab.td.get("/api/targets/box").await;
    assert_eq!(t.ok()["host_online"], true);
    let sid = exec(
        &lab.td,
        "box",
        json!({"cmd": ["sh", "-c", "echo out; echo err >&2; printf \"$GREETING\"; exit 3"],
               "env": {"GREETING": "hi"}, "cwd": "/"}),
    )
    .await;
    let d = drain(&lab.td, "box", &sid).await;
    assert_eq!(String::from_utf8_lossy(&d.stdout), "out\nhi");
    assert_eq!(String::from_utf8_lossy(&d.stderr), "err\n");
    assert_eq!(d.end["type"], "exit");
    assert_eq!(d.end["code"], 3);
    let audit = lab.td.get("/api/audit?action=target.exec").await;
    let row = &audit.ok()[0];
    assert_eq!(row["target"], "box");
    assert!(
        row["detail"]["command"]
            .as_str()
            .unwrap()
            .starts_with("sh -c echo out")
    );
    assert_eq!(row["detail"]["env"], json!(["GREETING"]));
    // Large output goes through the acks and the buffer intact.
    let sid = exec(
        &lab.td,
        "box",
        json!({"cmd": ["sh", "-c", "head -c 3000000 /dev/zero"]}),
    )
    .await;
    let d = drain(&lab.td, "box", &sid).await;
    assert_eq!(d.stdout.len(), 3_000_000);
    assert_eq!(d.end["code"], 0);
    // An unknown session is a 404.
    lab.td
        .get("/api/targets/box/sessions/nope/output")
        .await
        .err(404, "not_found");
}

#[tokio::test]
async fn pty_exec_takes_input_and_resize() {
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    lab.target(json!({"name": "box", "kind": "host", "host": "lab1"}))
        .await;
    let sid = exec(
        &lab.td,
        "box",
        json!({"cmd": ["sh", "-c", "stty -echo; read x; echo got:$x; stty size"],
               "pty": {"rows": 30, "cols": 100}}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    input(
        &lab.td,
        "box",
        &sid,
        json!({"resize": {"rows": 40, "cols": 120}}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    input(
        &lab.td,
        "box",
        &sid,
        json!({"data": B64.encode(b"hello\r")}),
    )
    .await;
    let d = drain(&lab.td, "box", &sid).await;
    let out = String::from_utf8_lossy(&d.stdout);
    assert!(out.contains("got:hello"), "{out:?}");
    assert!(out.contains("40 120"), "{out:?}");
    assert_eq!(d.end["code"], 0);
    // A login shell: `shell` opens a PTY with the host's shell.
    let sid = exec(&lab.td, "box", json!({"shell": true})).await;
    input(
        &lab.td,
        "box",
        &sid,
        json!({"data": B64.encode(b"echo shell-$((6*7)); exit 0\r")}),
    )
    .await;
    let d = drain(&lab.td, "box", &sid).await;
    assert!(String::from_utf8_lossy(&d.stdout).contains("shell-42"));
}

fn sha(b: &[u8]) -> String {
    terrarium_host::protocol::hex(&Sha256::digest(b))
}

async fn push(
    td: &TestDaemon,
    target: &str,
    path: &str,
    data: &[u8],
    claimed: Option<&str>,
) -> Value {
    let r = td
        .post(
            &format!("/api/targets/{target}/cp"),
            json!({"direction": "push", "path": path, "mode": 0o600}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let sid = r.body["session_id"].as_str().unwrap().to_owned();
    for chunk in data.chunks(64 * 1024) {
        input(td, target, &sid, json!({"data": B64.encode(chunk)})).await;
    }
    let digest = claimed.map_or_else(|| sha(data), str::to_owned);
    input(
        td,
        target,
        &sid,
        json!({"finish": {"bytes": data.len(), "sha256": digest}}),
    )
    .await;
    drain(td, target, &sid).await.end
}

#[tokio::test]
async fn cp_both_ways_with_sha256() {
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    lab.target(json!({"name": "box", "kind": "host", "host": "lab1"}))
        .await;
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("sub/f.bin");
    let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let end = push(&lab.td, "box", dst.to_str().unwrap(), &data, None).await;
    assert_eq!(end["type"], "cp", "{end}");
    assert_eq!(end["sha256"], sha(&data));
    assert_eq!(std::fs::read(&dst).unwrap(), data);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&dst).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // A wrong digest: the host refuses, the old file stays.
    let end = push(&lab.td, "box", dst.to_str().unwrap(), b"other", Some("00")).await;
    assert_eq!(end["type"], "error");
    assert_eq!(end["code"], "checksum_mismatch");
    assert_eq!(std::fs::read(&dst).unwrap(), data);

    // Pull it back.
    let r = lab
        .td
        .post(
            "/api/targets/box/cp",
            json!({"direction": "pull", "path": dst.to_str().unwrap()}),
        )
        .await;
    let sid = r.ok()["session_id"].as_str().unwrap().to_owned();
    let d = drain(&lab.td, "box", &sid).await;
    assert_eq!(d.file, data);
    assert_eq!(d.end["type"], "cp");
    assert_eq!(d.end["sha256"], sha(&data));
    assert_eq!(d.end["bytes"], data.len());
    // A relative path is refused before anything runs.
    lab.td
        .post(
            "/api/targets/box/cp",
            json!({"direction": "pull", "path": "rel/x"}),
        )
        .await
        .err(422, "invalid");
}

#[tokio::test]
async fn reconnect_resumes_a_running_session() {
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    lab.target(json!({"name": "box", "kind": "host", "host": "lab1"}))
        .await;
    let sid = exec(
        &lab.td,
        "box",
        json!({"cmd": ["sh", "-c", "echo one; sleep 1; echo two; exit 4"]}),
    )
    .await;
    // Wait for the first line, then cut the link.
    let path = format!("/api/targets/box/sessions/{sid}/output?since=0&wait=5");
    let first = lab.td.get(&path).await;
    assert!(!first.ok()["chunks"].as_array().unwrap().is_empty());
    assert!(lab.state().executor.hosts.drop_link("lab1"));
    let d = drain(&lab.td, "box", &sid).await;
    assert_eq!(String::from_utf8_lossy(&d.stdout), "one\ntwo\n");
    assert_eq!(d.end["code"], 4);
    assert_eq!(events(&lab.td, "host.disconnected").await.len(), 1);
    let connected = events(&lab.td, "host.connected").await;
    assert_eq!(connected.len(), 2);
    assert_eq!(connected[1]["data"]["resumed"], 1);
}

#[tokio::test]
async fn a_revoked_token_is_refused() {
    let lab = Lab::start().await;
    let token = lab.register("lab1").await;
    let task = tokio::spawn(lab.client("lab1", &token).run());
    lab.wait_online("lab1", true).await;
    lab.td.delete("/api/hosts/lab1").await;
    assert_eq!(lab.td.get("/api/hosts").await.body, json!([]));
    assert!(!lab.state().executor.hosts.is_online("lab1"));
    let audit = lab.td.get("/api/audit?action=host.revoke").await;
    assert_eq!(audit.ok().as_array().unwrap().len(), 1);
    // A dial with the old token gets 401 on the upgrade.
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = format!("ws://{}/hosts/connect", lab.addr)
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    let err = tokio_tungstenite::connect_async(req).await.unwrap_err();
    match err {
        tokio_tungstenite::tungstenite::Error::Http(resp) => assert_eq!(resp.status(), 401),
        other => panic!("expected 401, got {other}"),
    }
    // The client keeps retrying and stays offline.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!lab.state().executor.hosts.is_online("lab1"));
    task.abort();
}

#[tokio::test]
async fn agent_policy_limits_the_targets() {
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    lab.td.project("terra").await;
    lab.td.project("ops").await;
    let worker = lab
        .td
        .agent(
            "terra",
            "worker",
            json!({"policy": {"targets": ["box", "opsbox"]}}),
        )
        .await;
    let other = lab.td.agent("terra", "other", json!({})).await;
    let (w, o) = (
        worker["id"].as_str().unwrap().to_owned(),
        other["id"].as_str().unwrap().to_owned(),
    );
    lab.target(json!({"name": "box", "kind": "host", "host": "lab1", "project": "terra"}))
        .await;
    lab.target(json!({"name": "opsbox", "kind": "host", "host": "lab1", "project": "ops"}))
        .await;
    let body = json!({"cmd": ["true"]});
    lab.td
        .post_as(&o, "/api/targets/box/exec", body.clone())
        .await
        .err(403, "forbidden");
    // In the policy, but in another project.
    lab.td
        .post_as(&w, "/api/targets/opsbox/exec", body.clone())
        .await
        .err(403, "forbidden");
    let r = lab.td.post_as(&w, "/api/targets/box/exec", body).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let sid = r.body["session_id"].as_str().unwrap().to_owned();
    // Another agent cannot read the worker's session.
    lab.td
        .get_as(&o, &format!("/api/targets/box/sessions/{sid}/output"))
        .await
        .err(404, "not_found");
    let d = drain_as(&lab.td, Some(&w), "box", &sid).await;
    assert_eq!(d.end["code"], 0);
    // `targets ls` for an agent: `usable` marks what its policy allows.
    let mine = lab.td.get_as(&w, "/api/targets?usable=true").await;
    let names: Vec<&str> = mine
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["box"]);
    assert_eq!(
        lab.td.get_as(&o, "/api/targets?usable=true").await.body,
        json!([])
    );
    // A member cannot define a target; the operator can.
    lab.td
        .post_as(
            &w,
            "/api/targets",
            json!({"name": "x", "kind": "host", "host": "lab1", "project": "terra"}),
        )
        .await
        .err(403, "forbidden");
    // The per-agent cap on live sessions.
    let mut sessions = Vec::new();
    for _ in 0..terrariumd::executor::MAX_SESSIONS_PER_AGENT {
        let r = lab
            .td
            .post_as(&w, "/api/targets/box/exec", json!({"cmd": ["sleep", "5"]}))
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
        sessions.push(r.body["session_id"].as_str().unwrap().to_owned());
    }
    lab.td
        .post_as(&w, "/api/targets/box/exec", json!({"cmd": ["true"]}))
        .await
        .err(429, "rate_limited");
    let r = lab
        .td
        .req(
            hyper::Method::DELETE,
            &format!("/api/targets/box/sessions/{}", sessions[0]),
            None,
            Some(&w),
        )
        .await;
    assert_eq!(r.status, 204);
}

#[tokio::test]
async fn long_poll_sessions_through_the_agent_tunnel() {
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    lab.td.project("terra").await;
    let worker = lab
        .td
        .agent("terra", "worker", json!({"policy": {"targets": ["box"]}}))
        .await;
    lab.target(json!({"name": "box", "kind": "host", "host": "lab1"}))
        .await;
    let state = lab.state();
    let rpc = terrariumd::agent_rpc::AgentRpc::new(Arc::downgrade(&state));
    let ctx = RequestContext {
        agent: worker["id"].as_str().unwrap().parse().unwrap(),
        name: "worker".into(),
        project: "terra".into(),
        roles: vec![],
        harness: terrarium_core::HarnessId::Claude,
        status: terrarium_core::AgentStatus::Idle,
    };
    let call = |params: Value| {
        let rpc = &rpc;
        let ctx = &ctx;
        async move { rpc.handle(ctx, "http", params).await.unwrap() }
    };
    let r = call(json!({"method": "POST", "path": "/api/targets/box/exec",
                        "body": {"cmd": ["sh", "-c", "read x; sleep 0.3; echo late:$x"]}}))
    .await;
    assert_eq!(r["status"], 201, "{r}");
    let sid = r["body"]["session_id"].as_str().unwrap().to_owned();
    let base = format!("/api/targets/box/sessions/{sid}");
    let r = call(json!({"method": "POST", "path": format!("{base}/input"),
                        "body": {"data": B64.encode(b"x1\n"), "eof": true}}))
    .await;
    assert_eq!(r["status"], 200, "{r}");
    // The long-poll holds until the output comes.
    let started = std::time::Instant::now();
    let r = call(json!({"method": "GET", "path": format!("{base}/output"),
                        "query": {"since": 0, "wait": 10}}))
    .await;
    assert_eq!(r["status"], 200, "{r}");
    let chunks = r["body"]["chunks"].as_array().unwrap();
    assert!(!chunks.is_empty(), "{r}");
    assert!(started.elapsed() >= Duration::from_millis(200));
    let text = B64.decode(chunks[0]["data"].as_str().unwrap()).unwrap();
    assert_eq!(text, b"late:x1\n");
    // A wait=0 read after the end returns the end at once.
    let mut since = r["body"]["next"].as_u64().unwrap();
    loop {
        let r = call(json!({"method": "GET", "path": format!("{base}/output"),
                            "query": {"since": since, "wait": 5}}))
        .await;
        since = r["body"]["next"].as_u64().unwrap();
        if r["body"]["done"] == true {
            assert_eq!(r["body"]["end"]["code"], 0);
            break;
        }
    }
}

#[tokio::test]
async fn lifecycle_verbs_map_to_docker_and_vmlab() {
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    lab.target(json!({"name": "dbox", "kind": "docker", "host": "lab1",
                      "image": "alpine:3", "ephemeral": true}))
        .await;
    let s = lab.td.post_empty("/api/targets/dbox/status").await;
    assert_eq!(s.ok()["state"], "absent");
    // ensure_up is what the supervisor calls on agent start.
    let up = terrariumd::executor::ensure_up(&lab.state(), "dbox")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(up["state"], "running");
    assert_eq!(
        lab.td.get("/api/targets/dbox").await.ok()["state"],
        "running"
    );
    let r = lab.td.post_empty("/api/targets/dbox/up").await;
    assert_eq!(r.ok()["state"], "running");
    let r = lab.td.post_empty("/api/targets/dbox/down").await;
    assert_eq!(r.ok()["state"], "absent", "ephemeral: down removes it");
    lab.td
        .post_empty("/api/targets/dbox/snapshot")
        .await
        .err(422, "invalid");
    let log = lab.log();
    assert!(
        log.contains("docker run -d --name terrarium-dbox --label terrarium.target=dbox alpine:3 sleep infinity"),
        "{log}"
    );
    assert!(log.contains("docker rm -f -v terrarium-dbox"), "{log}");
    assert!(
        log.contains("docker inspect --type container terrarium-dbox"),
        "{log}"
    );

    let labdir = tempfile::tempdir().unwrap();
    std::fs::write(labdir.path().join("vmlab.wcl"), "lab \"win\" {}\n").unwrap();
    lab.target(json!({"name": "win", "kind": "vmlab", "host": "lab1",
                      "lab": labdir.path(), "machine": "win11"}))
        .await;
    assert_eq!(
        lab.td.post_empty("/api/targets/win/status").await.ok()["state"],
        "stopped"
    );
    assert_eq!(
        lab.td.post_empty("/api/targets/win/up").await.ok()["state"],
        "running"
    );
    let r = lab
        .td
        .post("/api/targets/win/snapshot", json!({"name": "clean"}))
        .await;
    assert_eq!(r.ok()["snapshot"], "clean");
    lab.td
        .post(
            "/api/targets/win/restore",
            json!({"name": "clean", "discard_guest_changes": true}),
        )
        .await
        .ok();
    assert_eq!(
        lab.td.get("/api/targets/win").await.ok()["snapshots"],
        json!(["clean"])
    );
    assert_eq!(
        lab.td.post_empty("/api/targets/win/down").await.ok()["state"],
        "stopped"
    );
    let log = lab.log();
    let dir = labdir.path().canonicalize().unwrap();
    for want in [
        "vmlab status".to_owned(),
        format!("vmlab up win11 (in {})", dir.display()),
        "vmlab snapshot create --vm win11 clean".to_owned(),
        "vmlab snapshot restore --vm win11 --discard-guest-changes clean".to_owned(),
        "vmlab down win11".to_owned(),
    ] {
        assert!(log.contains(&want), "missing {want:?} in\n{log}");
    }
    // maybe_down leaves a permanent target alone.
    assert!(
        terrariumd::executor::maybe_down(&lab.state(), "win")
            .await
            .unwrap()
            .is_none()
    );
    let audit = lab.td.get("/api/audit?action=target.up").await;
    assert!(audit.ok().as_array().unwrap().len() >= 2);
}

const FILE_TARGET: &str = "kind = \"ssh\"\naddress = \"build@host\"\nproject = \"terra\"\n";

#[tokio::test]
async fn target_files_sync_and_api_targets() {
    let mut lab = Lab::with_config(|c| {
        write(c, "projects/terra.toml", "name = \"terra\"\n");
        write(c, "targets/buildbox.toml", FILE_TARGET);
    })
    .await;
    let t = lab.td.get("/api/targets/buildbox").await;
    assert_eq!(t.ok()["source"], "file");
    assert_eq!(t.body["kind"], "ssh");
    assert_eq!(t.body["host_online"], Value::Null);
    lab.td
        .patch("/api/targets/buildbox", json!({"port": 2222}))
        .await
        .err(409, "file_managed");
    lab.td
        .delete("/api/targets/buildbox")
        .await
        .err(409, "file_managed");
    // An API target: create, patch, show in the project, delete.
    lab.target(json!({"name": "box", "kind": "host", "host": "lab1", "project": "terra"}))
        .await;
    lab.td
        .post(
            "/api/targets",
            json!({"name": "box", "kind": "host", "host": "x"}),
        )
        .await
        .err(409, "conflict");
    lab.td
        .post(
            "/api/targets",
            json!({"name": "bad", "kind": "docker", "host": "x"}),
        )
        .await
        .err(422, "invalid");
    let r = lab
        .td
        .patch("/api/targets/box", json!({"host": "lab2"}))
        .await;
    assert_eq!(r.ok()["host"], "lab2");
    let names: Vec<String> = lab
        .td
        .get("/api/projects/terra/targets")
        .await
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["box", "buildbox"]);
    assert_eq!(lab.td.delete("/api/targets/box").await.status, 204);
    // The file goes: the target goes on reload.
    std::fs::remove_file(lab.td.config.join("targets/buildbox.toml")).unwrap();
    lab.td.post_empty("/api/reload").await.ok();
    lab.td
        .get("/api/targets/buildbox")
        .await
        .err(404, "not_found");
    let _ = &mut lab;
}

fn git(dir: &Path, args: &[&str]) {
    let st = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(
        st.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&st.stderr)
    );
}

#[tokio::test]
async fn repo_checkout_runs_on_the_target() {
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    lab.td.project("terra").await;
    let src = tempfile::tempdir().unwrap();
    git(src.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(src.path().join("README"), "hi\n").unwrap();
    git(src.path(), &["add", "."]);
    git(src.path(), &["commit", "-q", "-m", "one"]);
    let url = src.path().display().to_string();
    let repo = lab
        .td
        .post(
            "/api/projects/terra/repos",
            json!({"url": url, "name": "app"}),
        )
        .await;
    let repo_id = repo.ok()["id"].as_str().unwrap().to_owned();
    lab.target(json!({"name": "box", "kind": "host", "host": "lab1"}))
        .await;
    let out = tempfile::tempdir().unwrap();
    let dest: PathBuf = out.path().join("app");
    let body = json!({"target": "box", "path": dest, "branch": "main"});
    let r = lab
        .td
        .post(&format!("/api/repos/{repo_id}/checkout"), body.clone())
        .await;
    let v = r.ok();
    assert_eq!(v["action"], "cloned", "{v}");
    assert_eq!(v["target"], "box");
    assert_eq!(v["branch"], "main");
    assert_eq!(v["head"].as_str().unwrap().len(), 40);
    assert_eq!(
        std::fs::read_to_string(dest.join("README")).unwrap(),
        "hi\n"
    );
    let r = lab
        .td
        .post(&format!("/api/repos/{repo_id}/checkout"), body.clone())
        .await;
    assert_eq!(r.ok()["action"], "up_to_date");
    std::fs::write(src.path().join("README"), "two\n").unwrap();
    git(src.path(), &["commit", "-q", "-am", "two"]);
    let r = lab
        .td
        .post(&format!("/api/repos/{repo_id}/checkout"), body)
        .await;
    assert_eq!(r.ok()["action"], "updated");
    assert_eq!(
        std::fs::read_to_string(dest.join("README")).unwrap(),
        "two\n"
    );
    let audit = lab.td.get("/api/audit?action=repo.checkout").await;
    assert_eq!(audit.ok()[0]["detail"]["target"], "box");
    // An agent needs the target in its policy.
    let a = lab.td.agent("terra", "worker", json!({})).await;
    lab.td
        .post_as(
            a["id"].as_str().unwrap(),
            &format!("/api/repos/{repo_id}/checkout"),
            json!({"target": "box", "path": dest}),
        )
        .await
        .err(403, "forbidden");
}

#[tokio::test]
async fn operator_terminal_websocket() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    lab.target(json!({"name": "box", "kind": "host", "host": "lab1"}))
        .await;
    let sid = exec(
        &lab.td,
        "box",
        json!({"cmd": ["sh", "-c", "read x; echo ws:$x"], "pty": true}),
    )
    .await;
    let mut ws = lab
        .td
        .ws(&format!("/api/targets/box/sessions/{sid}/ws"), None)
        .await
        .unwrap();
    ws.send(Message::Binary(b"abc\r".to_vec().into()))
        .await
        .unwrap();
    let mut seen = Vec::new();
    while let Ok(Some(Ok(m))) = tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
        match m {
            Message::Binary(b) => seen.extend_from_slice(&b),
            Message::Close(_) => break,
            _ => {}
        }
        if String::from_utf8_lossy(&seen).contains("ws:abc") {
            break;
        }
    }
    assert!(String::from_utf8_lossy(&seen).contains("ws:abc"));
}
