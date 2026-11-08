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
use terrarium_core::limits::LimitsCapability;
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
        self.client_with(name, token, None)
    }

    /// A host agent with this `limits` capability (`None`: an older host
    /// agent).
    fn client_with(&self, name: &str, token: &str, limits: Option<LimitsCapability>) -> Client {
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
            limits,
            ..Default::default()
        };
        o.backoff_min = Duration::from_millis(50);
        o.backoff_max = Duration::from_millis(200);
        o.docker_bin = self.bins.path().join("docker").display().to_string();
        o.vmlab_bin = self.bins.path().join("vmlab").display().to_string();
        // The daemon's work root is the host agent's `work_dir`, as on one
        // machine.
        o.paths = terrarium_host::HostPaths {
            work_dir: self.td.state.with_file_name("work"),
            instances_dir: self.bins.path().join("instances"),
            repos_dir: self.bins.path().join("repos"),
        };
        Client::new(o)
    }

    /// Register `name`, run its host agent, and wait for it to be online.
    async fn host(&self, name: &str) -> tokio::task::JoinHandle<()> {
        self.host_with(name, None).await
    }

    /// [`Lab::host`] with a `limits` capability.
    async fn host_with(
        &self,
        name: &str,
        limits: Option<LimitsCapability>,
    ) -> tokio::task::JoinHandle<()> {
        let token = self.register(name).await;
        let task = tokio::spawn(self.client_with(name, &token, limits).run());
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
            r#"for a in "$@"; do c=$a; done
if [ "$1" = run ]; then prev=; for a in "$@"; do if [ "$prev" = --name ]; then c=$a; fi; prev=$a; done; fi
f='{s}'.$c
case "$1" in
  inspect) if [ -f "$f" ]; then st=$(cat "$f"); mounts='[]'; image=alpine:3; hc='{{}}'; if [ -f '{mounts}' ]; then mounts=$(cat '{mounts}'); fi; if [ -f '{image}' ]; then image=$(cat '{image}'); fi; if [ -f '{hc}' ]; then hc=$(cat '{hc}'); fi; echo "[{{\"Id\":\"abc\",\"Name\":\"/terrarium-dbox\",\"Config\":{{\"Image\":\"$image\"}},\"HostConfig\":$hc,\"Mounts\":$mounts,\"State\":{{\"Status\":\"$st\"}}}}]"; else echo '[]'; echo 'Error: No such object' >&2; exit 1; fi ;;
  run) echo running > "$f"; echo abc123 ;;
  start) echo running > "$f"; echo "$2" ;;
  stop) echo exited > "$f"; echo "$4" ;;
  rm) rm -f "$f" ;;
  exec) for a in "$@"; do if [ "$a" = sleeper ]; then i=0; while [ ! -f '{end}' ] && [ $i -lt 1200 ]; do sleep 0.05; i=$((i+1)); done; fi; done ;;
esac"#,
            s = state.display(),
            mounts = dir.join("docker.mounts").display(),
            image = dir.join("docker.image").display(),
            hc = dir.join("docker.host-config").display(),
            end = dir.join("sleeper.end").display()
        ),
        _ => format!(
            r#"case "$1" in
  up) if [ -f '{slow}' ]; then sleep 1; fi; touch '{s}' ;;
  down|destroy) rm -f '{s}' ;;
  status) if [ -f '{gone}' ]; then echo 'lab "win": not running'; exit 0; fi
          printf 'lab "win"\n\n  NAME     KIND      STATUS   IP           TEMPLATE/IMAGE\n'
          if [ -f '{s}' ]; then st=running; else st=stopped; fi
          printf '  win11    vm        %s  10.0.0.9     x86_64/win11\n' "$st"
          if [ -f '{two}' ]; then printf '  linux01  vm        %s  10.0.0.8     x86_64/ubuntu\n' "$st"; fi
          if [ -f '{box}' ]; then printf '  box      container running  -            alpine:3.22\n'; fi ;;
  snapshot) sub=$2; shift 2; vm=; pos=
    while [ $# -gt 0 ]; do case "$1" in --vm) vm=$2; shift 2 ;; --*) shift ;; *) pos="$pos $1"; shift ;; esac; done
    set -- $pos
    touch '{snaps}'
    case "$sub" in
      create) ms=$vm; if [ -z "$ms" ]; then ms=win11; if [ -f '{two}' ]; then ms="win11 linux01"; fi; fi
              for m in $ms; do grep -qx "$m $1" '{snaps}' || echo "$m $1" >> '{snaps}'; done ;;
      list) if grep -q "^$1 " '{snaps}'; then echo 'NAME              KIND     TAKEN'
              grep "^$1 " '{snaps}' | while read m n; do printf '%-17s online   2026-10-26T08:00:00Z\n' "$n"; done
            else echo "no snapshots for $1"; fi ;;
      delete) if grep -qx "$1 $2" '{snaps}'; then grep -vx "$1 $2" '{snaps}' > '{snaps}.tmp'; mv '{snaps}.tmp' '{snaps}'
              else echo "\"$1\" has no snapshot \"$2\"" >&2; exit 1; fi ;;
    esac ;;
esac"#,
            s = state.display(),
            slow = dir.join("slow").display(),
            gone = dir.join("vmlab.gone").display(),
            two = dir.join("two").display(),
            box = dir.join("box").display(),
            snaps = dir.join("vmlab.snaps").display()
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
    // Built with the daemon: the same version, so not stale.
    assert_eq!(h["version"], terrarium_version::FULL);
    assert_eq!(h["stale"], false);
    let ev = events(&lab.td, "host.connected").await;
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0]["data"]["host"], "lab1");
    task.abort();
}

/// A host agent from another build: `stale` in the list, while it is
/// online and after it goes offline (the row keeps the version).
#[tokio::test]
async fn a_host_agent_of_another_ref_is_stale() {
    let lab = Lab::start().await;
    let token = lab.register("old").await;
    let mut o = ClientOptions::new(format!("ws://{}/hosts/connect", lab.addr), &token, "old");
    o.version = "0.0.9+0000000".into();
    o.backoff_min = Duration::from_millis(50);
    o.backoff_max = Duration::from_millis(200);
    let task = tokio::spawn(Client::new(o).run());
    lab.wait_online("old", true).await;
    let h = lab.td.get("/api/hosts/old").await;
    let h = h.ok();
    assert_eq!(h["version"], "0.0.9+0000000");
    // The test build knows its ref unless git is missing; then nothing
    // compares.
    let known = terrarium_version::this().git_ref.is_some();
    assert_eq!(h["stale"], known);
    task.abort();
    lab.wait_online("old", false).await;
    let listed = lab.td.get("/api/hosts").await;
    assert_eq!(listed.ok()[0]["stale"], known);
    // Any caller reads the daemon build, an agent too.
    lab.td.project("ops").await;
    lab.td.agent("ops", "worker", json!({})).await;
    let v = lab.td.get_as("worker", "/api/system/version").await;
    assert_eq!(v.ok()["version"], terrarium_version::FULL);
}

/// A fake host agent binary: a script that prints its version.
fn fake_host_binary(version: &str) -> String {
    format!("#!/bin/sh\necho 'terrarium-host {version}'\n")
}

fn write_exec(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Self-update (docs/host-protocol.md, "Self-update"): a stale host agent
/// gets the daemon's binary in `welcome.update`, downloads it from the
/// host listener, checks it, swaps it in, and stops; the new build then
/// connects, and the daemon records `host.updated` and the audit
/// `host.update`. A failed update that a host agent reports is recorded
/// once.
#[tokio::test]
async fn a_stale_host_agent_updates_itself_from_the_host_listener() {
    if terrarium_version::this().git_ref.is_none() {
        return; // no git: nothing is stale, so nothing is offered
    }
    let dist = tempfile::tempdir().unwrap();
    let served = fake_host_binary(terrarium_version::FULL);
    write_exec(
        &dist
            .path()
            .join(format!("terrarium-host-{}-linux", std::env::consts::ARCH)),
        &served,
    );
    let mut td = TestDaemon::start().await;
    td.web.install.dist_dir = dist.path().to_owned();
    td.boot(None, None).await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    let (_task, addr) = terrariumd::hosts::serve(state, "127.0.0.1:0")
        .await
        .unwrap();

    // The host listener serves the binary and its SHA-256.
    let raw = {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(
            format!(
                "GET /install/terrarium-host.sha256?arch={} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
                std::env::consts::ARCH
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        let mut b = Vec::new();
        s.read_to_end(&mut b).await.unwrap();
        b
    };
    let body = terrarium_host::selfupdate::parse_response(&raw).unwrap();
    assert!(
        String::from_utf8(body)
            .unwrap()
            .starts_with(&sha(served.as_bytes()))
    );

    let r = td.post("/api/hosts", json!({"name": "upd"})).await;
    let token = r.body["token"].as_str().unwrap().to_owned();
    let bin_dir = tempfile::tempdir().unwrap();
    let bin = bin_dir.path().join("terrarium-host");
    let old = fake_host_binary("0.0.9+0000000");
    write_exec(&bin, &old);
    let options = |version: &str| {
        let mut o = ClientOptions::new(format!("ws://{addr}/hosts/connect"), &token, "upd");
        o.version = version.into();
        o.backoff_min = Duration::from_millis(50);
        o.backoff_max = Duration::from_millis(200);
        o.self_update = Some(terrarium_host::SelfUpdateOptions {
            binary: bin.clone(),
            enabled: true,
            supervised: true,
            idle_poll: Duration::from_millis(20),
        });
        o
    };
    // The old build updates itself, and its run ends.
    tokio::time::timeout(
        Duration::from_secs(30),
        Client::new(options("0.0.9+0000000")).run(),
    )
    .await
    .expect("the host agent stops after the update");
    use terrarium_host::selfupdate::{PENDING, PREV, sidecar};
    assert_eq!(std::fs::read_to_string(&bin).unwrap(), served);
    assert_eq!(std::fs::read_to_string(sidecar(&bin, PREV)).unwrap(), old);
    assert_eq!(
        std::fs::read_to_string(sidecar(&bin, PENDING)).unwrap(),
        format!("{}\n", terrarium_version::FULL)
    );

    // "systemd" starts the new build: it confirms the update, and the
    // daemon records it.
    let task = tokio::spawn(Client::new(options(terrarium_version::FULL)).run());
    let mut found = Vec::new();
    for _ in 0..300 {
        found = events(&td, "host.updated").await;
        if !found.is_empty() && !sidecar(&bin, PENDING).exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0]["data"]["from"], "0.0.9+0000000");
    assert_eq!(found[0]["data"]["to"], terrarium_version::FULL);
    assert!(!sidecar(&bin, PENDING).exists());
    let a = last_audit(&td, "host.update").await;
    assert_eq!(a["detail"]["to"], terrarium_version::FULL);
    let h = td.get("/api/hosts/upd").await;
    assert_eq!(h.ok()["stale"], false);
    task.abort();

    // A host agent that went back to its old binary says so in `hello`.
    std::fs::write(
        sidecar(&bin, terrarium_host::selfupdate::FAILED),
        "0.9.9+fffffff\n",
    )
    .unwrap();
    // Two links: the daemon records the failure after the first `hello`
    // only. It writes the records before `host.connected`.
    for _ in 0..2 {
        let before = events(&td, "host.connected").await.len();
        let mut o = options(terrarium_version::FULL);
        o.self_update.as_mut().unwrap().enabled = false;
        let task = tokio::spawn(Client::new(o).run());
        for _ in 0..300 {
            if events(&td, "host.connected").await.len() > before {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        task.abort();
        let _ = task.await;
    }
    let failed = events(&td, "host.update_failed").await;
    assert_eq!(failed.len(), 1, "recorded once: {failed:?}");
    assert_eq!(failed[0]["data"]["failed"], "0.9.9+fffffff");
    let a = last_audit(&td, "host.update_failed").await;
    assert_eq!(a["detail"]["failed"], "0.9.9+fffffff");
}

/// A host agent with a bad download (the daemon's binary does not match
/// its own SHA-256) keeps its binary and keeps running.
#[tokio::test]
async fn a_self_update_with_a_wrong_checksum_changes_nothing() {
    if terrarium_version::this().git_ref.is_none() {
        return;
    }
    let dist = tempfile::tempdir().unwrap();
    let name = format!("terrarium-host-{}-linux", std::env::consts::ARCH);
    write_exec(
        &dist.path().join(&name),
        &fake_host_binary(terrarium_version::FULL),
    );
    std::fs::write(
        dist.path().join(format!("{name}.sha256")),
        format!("{}  {name}\n", "0".repeat(64)),
    )
    .unwrap();
    let mut td = TestDaemon::start().await;
    td.web.install.dist_dir = dist.path().to_owned();
    td.boot(None, None).await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    let (_task, addr) = terrariumd::hosts::serve(state, "127.0.0.1:0")
        .await
        .unwrap();
    let r = td.post("/api/hosts", json!({"name": "bad"})).await;
    let token = r.body["token"].as_str().unwrap().to_owned();
    let bin_dir = tempfile::tempdir().unwrap();
    let bin = bin_dir.path().join("terrarium-host");
    let old = fake_host_binary("0.0.9+0000000");
    write_exec(&bin, &old);
    let mut o = ClientOptions::new(format!("ws://{addr}/hosts/connect"), &token, "bad");
    o.version = "0.0.9+0000000".into();
    o.self_update = Some(terrarium_host::SelfUpdateOptions {
        binary: bin.clone(),
        enabled: true,
        supervised: true,
        idle_poll: Duration::from_millis(20),
    });
    let run = tokio::spawn(Client::new(o).run());
    // The agent stays online and its binary stays.
    for _ in 0..300 {
        let h = td.get("/api/hosts/bad").await;
        if h.body["online"] == true {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !run.is_finished(),
        "a failed update must not stop the agent"
    );
    assert_eq!(std::fs::read_to_string(&bin).unwrap(), old);
    use terrarium_host::selfupdate::{NEW, PENDING, PREV, sidecar};
    for s in [NEW, PENDING, PREV] {
        assert!(!sidecar(&bin, s).exists(), "{s}");
    }
    run.abort();
}

#[tokio::test]
async fn metrics_reach_the_host_list_and_the_history() {
    let lab = Lab::start().await;
    lab.td.project("ops").await;
    let agent = lab.td.agent("ops", "worker", json!({})).await;
    let agent = agent["id"].as_str().unwrap().to_owned();

    // An older host agent: no `metrics` capability, so no samples.
    let old_token = lab.register("old").await;
    let old = tokio::spawn(lab.client("old", &old_token).run());
    lab.wait_online("old", true).await;
    let h = lab.td.get("/api/hosts/old").await;
    assert_eq!(h.ok()["metrics"], Value::Null);
    assert!(
        h.body["capabilities"].get("metrics").is_none(),
        "{}",
        h.body
    );
    let hist = lab.td.get("/api/hosts/old/metrics").await;
    assert_eq!(hist.ok()["samples"], json!([]));
    assert_eq!(hist.body["stale"], true);

    // A host agent with metrics, at a short interval for the test.
    let token = lab.register("lab1").await;
    let mut o = ClientOptions::new(format!("ws://{}/hosts/connect", lab.addr), token, "lab1");
    o.capabilities.metrics = true;
    o.metrics_interval = Duration::from_millis(50);
    o.backoff_min = Duration::from_millis(50);
    let task = tokio::spawn(Client::new(o).run());
    lab.wait_online("lab1", true).await;
    let mut latest = Value::Null;
    for _ in 0..200 {
        latest = lab.td.get("/api/hosts/lab1").await.body["metrics"].clone();
        if !latest.is_null() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(latest["at"].is_string(), "{latest}");
    assert_eq!(latest["stale"], false);
    assert!(latest["cpu"]["cores"].as_u64().unwrap() > 0, "{latest}");
    assert!(latest["memory"]["total_bytes"].as_u64().unwrap() > 0);
    assert!(latest["interval_secs"].as_f64().unwrap() > 0.0);
    let listed = lab.td.get("/api/hosts").await;
    let row = listed
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["name"] == "lab1")
        .unwrap();
    assert_eq!(row["capabilities"]["metrics"], true);
    assert!(row["metrics"].is_object());

    // The history: oldest first, and any caller may read it.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let hist = lab
        .td
        .get_as(&agent, "/api/hosts/lab1/metrics?minutes=5")
        .await;
    let hist = hist.ok();
    assert_eq!(hist["interval_secs"], 10);
    assert_eq!(hist["minutes"], 5);
    let samples = hist["samples"].as_array().unwrap();
    assert!(samples.len() >= 2, "{hist}");
    assert!(samples[0]["at"].as_str() <= samples[samples.len() - 1]["at"].as_str());
    for bad in ["0", "61", "x"] {
        let r = lab
            .td
            .get(&format!("/api/hosts/lab1/metrics?minutes={bad}"))
            .await;
        assert_eq!(r.status, 400, "{bad}: {}", r.body);
    }
    lab.td
        .get("/api/hosts/nope/metrics")
        .await
        .err(404, "not_found");

    // Offline: the samples stay, and they are stale.
    task.abort();
    lab.wait_online("lab1", false).await;
    let h = lab.td.get("/api/hosts/lab1").await;
    assert_eq!(h.ok()["metrics"]["stale"], true);
    old.abort();
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
    // After the end, an `eof` is a no-op (the CLI closes stdin after the
    // open, and a fast command can end first), but data is a 409.
    let r = lab
        .td
        .post(
            &format!("/api/targets/box/sessions/{sid}/input"),
            json!({"eof": true}),
        )
        .await;
    assert_eq!(r.ok()["accepted"], false);
    lab.td
        .post(
            &format!("/api/targets/box/sessions/{sid}/input"),
            json!({"data": "aGk="}),
        )
        .await
        .err(409, "conflict");
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

/// PLAN §10: host agent tokens are operator-only. A sysadmin agent may list
/// the hosts, but not register or revoke one.
#[tokio::test]
async fn host_tokens_are_operator_only() {
    let lab = Lab::start().await;
    lab.td.project("ops").await;
    let sys = lab
        .td
        .agent("ops", "lead", json!({"roles": ["sysadmin"]}))
        .await;
    let sys = sys["id"].as_str().unwrap().to_owned();
    lab.td
        .post_as(&sys, "/api/hosts", json!({"name": "lab1"}))
        .await
        .err(403, "forbidden");
    lab.register("lab1").await;
    let listed = lab.td.get_as(&sys, "/api/hosts").await;
    assert_eq!(listed.ok().as_array().unwrap().len(), 1);
    assert!(listed.body[0].get("token").is_none(), "{}", listed.body);
    lab.td.get_as(&sys, "/api/hosts/lab1").await.ok();
    let r = lab
        .td
        .req(hyper::Method::DELETE, "/api/hosts/lab1", None, Some(&sys))
        .await;
    r.err(403, "forbidden");
    assert_eq!(
        lab.td
            .get("/api/hosts")
            .await
            .body
            .as_array()
            .unwrap()
            .len(),
        1
    );
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
            json!({"policy": {"targets": ["terra-box", "ops-opsbox"]}}),
        )
        .await;
    let other = lab.td.agent("terra", "other", json!({})).await;
    let (w, o) = (
        worker["id"].as_str().unwrap().to_owned(),
        other["id"].as_str().unwrap().to_owned(),
    );
    lab.target(json!({"name": "terra-box", "kind": "host", "host": "lab1", "project": "terra"}))
        .await;
    lab.target(json!({"name": "ops-opsbox", "kind": "host", "host": "lab1", "project": "ops"}))
        .await;
    let body = json!({"cmd": ["true"]});
    lab.td
        .post_as(&o, "/api/targets/terra-box/exec", body.clone())
        .await
        .err(403, "forbidden");
    // In the policy, but in another project.
    lab.td
        .post_as(&w, "/api/targets/ops-opsbox/exec", body.clone())
        .await
        .err(403, "forbidden");
    let r = lab
        .td
        .post_as(&w, "/api/targets/terra-box/exec", body)
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let sid = r.body["session_id"].as_str().unwrap().to_owned();
    // Another agent cannot read the worker's session.
    lab.td
        .get_as(&o, &format!("/api/targets/terra-box/sessions/{sid}/output"))
        .await
        .err(404, "not_found");
    let d = drain_as(&lab.td, Some(&w), "terra-box", &sid).await;
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
    assert_eq!(names, ["terra-box"]);
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
            .post_as(
                &w,
                "/api/targets/terra-box/exec",
                json!({"cmd": ["sleep", "5"]}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
        sessions.push(r.body["session_id"].as_str().unwrap().to_owned());
    }
    lab.td
        .post_as(&w, "/api/targets/terra-box/exec", json!({"cmd": ["true"]}))
        .await
        .err(429, "rate_limited");
    let r = lab
        .td
        .req(
            hyper::Method::DELETE,
            &format!("/api/targets/terra-box/sessions/{}", sessions[0]),
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
    // The daemon's own verb on an agent start leaves an audit row too.
    let a = lab.td.get("/api/audit?action=target.up").await;
    let a = a.ok();
    let row = a.as_array().unwrap().last().unwrap();
    assert_eq!(row["actor"], "system", "{row}");
    assert_eq!(row["target"], "dbox");
    assert_eq!(row["detail"]["reason"], "agent_start");
    let r = lab.td.post_empty("/api/targets/dbox/up").await;
    assert_eq!(r.ok()["state"], "running");
    let r = lab.td.post_empty("/api/targets/dbox/down").await;
    assert_eq!(r.ok()["state"], "absent", "ephemeral: down removes it");
    // maybe_down (an agent stop) removes it and writes target.destroy.
    lab.td.post_empty("/api/targets/dbox/up").await.ok();
    let down = terrariumd::executor::maybe_down(&lab.state(), "dbox")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(down["state"], "absent");
    let a = lab.td.get("/api/audit?action=target.destroy").await;
    let a = a.ok();
    let row = a.as_array().unwrap().last().unwrap();
    assert_eq!(row["actor"], "system", "{row}");
    assert_eq!(row["detail"]["reason"], "agent_stop");
    lab.td
        .post_empty("/api/targets/dbox/snapshot")
        .await
        .err(422, "invalid");
    let log = lab.log();
    assert!(
        log.contains("docker run -d --init --name terrarium-dbox --label terrarium.target=dbox alpine:3 sleep infinity"),
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

fn names(v: &Value) -> Vec<String> {
    v["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["name"].as_str().unwrap().to_owned())
        .collect()
}

async fn delete_as(td: &TestDaemon, agent: &str, path: &str) -> common::Resp {
    td.req(hyper::Method::DELETE, path, None, Some(agent)).await
}

/// docs/api.md §17.3: `GET .../snapshots` lists what the host has, marks
/// what Terrarium took (`managed`), and drops the stored names that the
/// host lost; `DELETE .../snapshots/{snap}` deletes on the machine, or on
/// each machine of the lab that has it. `scope` picks the machine or the
/// lab. The rights are those of the verbs.
#[tokio::test]
async fn snapshots_list_delete_and_lab_scope() {
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    lab.td.project("terra").await;
    let user = id_of(
        &lab.td
            .agent(
                "terra",
                "user",
                json!({"policy": {"targets": ["terra-win"]}}),
            )
            .await,
    );
    let other = id_of(&lab.td.agent("terra", "other", json!({})).await);
    let labdir = tempfile::tempdir().unwrap();
    std::fs::write(labdir.path().join("vmlab.wcl"), "lab \"win\" {}\n").unwrap();
    lab.target(
        json!({"name": "win", "kind": "vmlab", "host": "lab1", "project": "terra",
                      "lab": labdir.path(), "machine": "win11"}),
    )
    .await;
    std::fs::write(lab.bins.path().join("two"), "").unwrap();
    lab.td.post_empty("/api/targets/terra-win/up").await.ok();

    // Take: the machine by default; `scope: lab` drops `--vm`.
    let r = lab
        .td
        .post("/api/targets/terra-win/snapshot", json!({"name": "a"}))
        .await;
    assert_eq!(r.ok()["scope"], "machine");
    let r = lab
        .td
        .post(
            "/api/targets/terra-win/snapshot",
            json!({"name": "b", "scope": "lab"}),
        )
        .await;
    assert_eq!(r.ok()["scope"], "lab");
    let log = lab.log();
    assert!(log.contains("vmlab snapshot create --vm win11 a"), "{log}");
    assert!(log.contains("vmlab snapshot create b"), "{log}");
    // A snapshot that Terrarium did not take.
    let snaps = lab.bins.path().join("vmlab.snaps");
    let mut text = std::fs::read_to_string(&snaps).unwrap();
    text.push_str("win11 foreign\n");
    std::fs::write(&snaps, &text).unwrap();

    let v = lab.td.get("/api/targets/terra-win/snapshots").await;
    let v = v.ok();
    assert_eq!(v["scope"], "machine");
    assert_eq!(v["machines"], json!(["win11"]));
    assert_eq!(names(v), ["a", "b", "foreign"]);
    let managed: Vec<bool> = v["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["managed"].as_bool().unwrap())
        .collect();
    assert_eq!(managed, [true, true, false]);
    assert_eq!(v["snapshots"][0]["kind"], "online");
    assert_eq!(v["snapshots"][0]["taken"], "2026-10-26T08:00:00Z");
    assert_eq!(v["dropped"], json!([]));
    assert!(lab.log().contains("vmlab snapshot list win11"));
    // The foreign one is not added to the stored names.
    assert_eq!(
        lab.td.get("/api/targets/terra-win").await.ok()["snapshots"],
        json!(["a", "b"])
    );

    // The lab: every machine that `vmlab status` names.
    let v = lab
        .td
        .get("/api/targets/terra-win/snapshots?scope=lab")
        .await;
    let v = v.ok();
    assert_eq!(v["lab_running"], true);
    assert_eq!(v["machines"], json!(["win11", "linux01"]));
    let b = v["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["name"] == "b")
        .unwrap();
    assert_eq!(b["machines"], json!(["win11", "linux01"]));

    // A name the host lost leaves the stored names, with an audit row and
    // an event.
    let text = std::fs::read_to_string(&snaps)
        .unwrap()
        .replace("win11 a\n", "");
    std::fs::write(&snaps, text).unwrap();
    let v = lab.td.get("/api/targets/terra-win/snapshots").await;
    assert_eq!(v.ok()["dropped"], json!(["a"]));
    assert_eq!(
        lab.td.get("/api/targets/terra-win").await.ok()["snapshots"],
        json!(["b"])
    );
    let row = last_audit(&lab.td, "target.reconcile").await;
    assert_eq!(row["detail"]["snapshots_dropped"], json!(["a"]), "{row}");
    assert!(
        events(&lab.td, "target")
            .await
            .iter()
            .any(|e| e["data"]["change"] == "snapshots_dropped")
    );

    // Delete lab-wide: each machine that has it.
    let r = lab
        .td
        .delete("/api/targets/terra-win/snapshots/b?scope=lab")
        .await;
    let r = r.ok();
    assert_eq!(r["machines"], json!(["win11", "linux01"]));
    assert_eq!(r["verb"], "snapshot_delete");
    let log = lab.log();
    assert!(log.contains("vmlab snapshot delete win11 b"), "{log}");
    assert!(log.contains("vmlab snapshot delete linux01 b"), "{log}");
    assert_eq!(
        lab.td.get("/api/targets/terra-win").await.ok()["snapshots"],
        json!([])
    );
    let row = last_audit(&lab.td, "target.snapshot_delete").await;
    assert_eq!(row["actor"], "operator");
    assert_eq!(row["detail"]["snapshot"], "b");
    assert!(
        events(&lab.td, "target")
            .await
            .iter()
            .any(|e| e["data"]["change"] == "snapshot_deleted" && e["data"]["snapshot"] == "b")
    );
    // A foreign snapshot can go too. A missing one is 404 for both scopes:
    // the daemon checks the list first, because vmlab 0.34 answers
    // "deleted" for a snapshot that does not exist.
    lab.td
        .delete("/api/targets/terra-win/snapshots/foreign")
        .await
        .ok();
    lab.td
        .delete("/api/targets/terra-win/snapshots/nosuch")
        .await
        .err(404, "not_found");
    lab.td
        .delete("/api/targets/terra-win/snapshots/nosuch?scope=lab")
        .await
        .err(404, "not_found");
    lab.td
        .delete("/api/targets/terra-win/snapshots/-x")
        .await
        .err(422, "invalid");

    // Rights: the agent that may use the target, not the other one.
    lab.td
        .get_as(&other, "/api/targets/terra-win/snapshots")
        .await
        .err(403, "forbidden");
    delete_as(&lab.td, &other, "/api/targets/terra-win/snapshots/x")
        .await
        .err(403, "forbidden");
    let v = lab
        .td
        .get_as(&user, "/api/targets/terra-win/snapshots")
        .await;
    assert_eq!(v.ok()["snapshots"], json!([]));
    lab.td
        .post_as(
            &user,
            "/api/targets/terra-win/snapshot",
            json!({"name": "mine"}),
        )
        .await
        .ok();
    let r = delete_as(&lab.td, &user, "/api/targets/terra-win/snapshots/mine").await;
    assert_eq!(r.ok()["machines"], json!(["win11"]));
    let row = last_audit(&lab.td, "target.snapshot_delete").await;
    assert_eq!(row["actor"], format!("agent:{user}"), "{row}");

    // `terrarium-clean` with `snapshot_before_task` needs force.
    lab.td
        .patch(
            "/api/targets/terra-win",
            json!({"snapshot_before_task": true}),
        )
        .await
        .ok();
    lab.td
        .post(
            "/api/targets/terra-win/snapshot",
            json!({"name": "terrarium-clean"}),
        )
        .await
        .ok();
    lab.td
        .delete("/api/targets/terra-win/snapshots/terrarium-clean")
        .await
        .err(409, "conflict");
    lab.td
        .delete("/api/targets/terra-win/snapshots/terrarium-clean?force=true")
        .await
        .ok();
    assert_eq!(
        lab.td.get("/api/targets/terra-win").await.ok()["snapshots"],
        json!([])
    );

    // A lab that does not run names no machines: a lab-wide list is empty
    // and drops nothing; a lab-wide delete is refused.
    lab.td
        .post("/api/targets/terra-win/snapshot", json!({"name": "keep"}))
        .await
        .ok();
    std::fs::write(lab.bins.path().join("vmlab.gone"), "").unwrap();
    let v = lab
        .td
        .get("/api/targets/terra-win/snapshots?scope=lab")
        .await;
    let v = v.ok();
    assert_eq!(v["lab_running"], false);
    assert_eq!(v["snapshots"], json!([]));
    assert_eq!(v["dropped"], json!([]));
    assert_eq!(
        lab.td.get("/api/targets/terra-win").await.ok()["snapshots"],
        json!(["keep"])
    );
    lab.td
        .delete("/api/targets/terra-win/snapshots/keep?scope=lab")
        .await
        .err(409, "conflict");
    std::fs::remove_file(lab.bins.path().join("vmlab.gone")).unwrap();

    // A target with no machine: the lab is the default scope, and
    // `machine` is refused.
    lab.target(json!({"name": "whole", "kind": "vmlab", "host": "lab1",
                      "lab": labdir.path()}))
        .await;
    let v = lab.td.get("/api/targets/whole/snapshots").await;
    assert_eq!(v.ok()["scope"], "lab");
    lab.td
        .get("/api/targets/whole/snapshots?scope=machine")
        .await
        .err(422, "invalid");
    lab.td
        .post(
            "/api/targets/whole/snapshot",
            json!({"name": "x", "scope": "machine"}),
        )
        .await
        .err(422, "invalid");
    lab.td
        .get("/api/targets/whole/snapshots?scope=planet")
        .await
        .err(400, "bad_request");
    // A docker target has no snapshots.
    lab.target(json!({"name": "dbox", "kind": "docker", "host": "lab1", "image": "alpine:3"}))
        .await;
    lab.td
        .get("/api/targets/dbox/snapshots")
        .await
        .err(422, "invalid");
}

/// `snapshot_before_task` (PLAN §19): a vmlab target gets the snapshot
/// `terrarium-clean` after `up` on an agent start, one time; a kept target
/// is restored to it on the stop; a destroy clears it; a permanent target
/// gets it once when it runs; a docker target gets nothing.
#[tokio::test]
async fn snapshot_before_task_takes_and_restores_terrarium_clean() {
    use terrariumd::executor::{ensure_up, maybe_down};
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    let state = lab.state();
    let labdir = tempfile::tempdir().unwrap();
    std::fs::write(labdir.path().join("vmlab.wcl"), "lab \"win\" {}\n").unwrap();
    lab.target(json!({"name": "win", "kind": "vmlab", "host": "lab1",
                      "lab": labdir.path(), "machine": "win11",
                      "ephemeral": true, "keep": true, "snapshot_before_task": true}))
        .await;
    let take = "vmlab snapshot create --vm win11 terrarium-clean";
    let restore = "vmlab snapshot restore --vm win11 terrarium-clean";

    // The first start: up, then the snapshot, in that order.
    ensure_up(&state, "win").await.unwrap().unwrap();
    let log = lab.log();
    let (up_at, take_at) = (log.find("vmlab up win11").unwrap(), log.find(take).unwrap());
    assert!(up_at < take_at, "{log}");
    assert_eq!(
        lab.td.get("/api/targets/win").await.ok()["snapshots"],
        json!(["terrarium-clean"])
    );
    let a = lab.td.get("/api/audit?action=target.snapshot").await;
    let a = a.ok();
    let row = a.as_array().unwrap().last().unwrap();
    assert_eq!(row["actor"], "system", "{row}");
    assert_eq!(row["target"], "win");
    assert_eq!(row["detail"]["reason"], "snapshot_before_task");
    assert_eq!(row["detail"]["snapshot"], "terrarium-clean");

    // The stop of a kept target restores the snapshot and does not destroy.
    let r = maybe_down(&state, "win").await.unwrap().unwrap();
    assert_eq!(r["verb"], "restore");
    let log = lab.log();
    assert_eq!(log.matches(restore).count(), 1, "{log}");
    assert!(!log.contains("vmlab destroy"), "{log}");
    let a = lab.td.get("/api/audit?action=target.restore").await;
    let a = a.ok();
    let row = a.as_array().unwrap().last().unwrap();
    assert_eq!(row["actor"], "system", "{row}");
    assert_eq!(row["detail"]["reason"], "agent_stop");

    // The next start finds the snapshot and takes no second one.
    ensure_up(&state, "win").await.unwrap().unwrap();
    maybe_down(&state, "win").await.unwrap().unwrap();
    let log = lab.log();
    assert_eq!(log.matches(take).count(), 1, "{log}");
    assert_eq!(log.matches(restore).count(), 2, "{log}");

    // Without keep: the stop destroys (no restore) and clears the snapshot,
    // so the next start takes a new one.
    lab.td
        .patch("/api/targets/win", json!({"keep": false}))
        .await
        .ok();
    let d = maybe_down(&state, "win").await.unwrap().unwrap();
    assert_eq!(d["state"], "absent");
    assert_eq!(
        lab.td.get("/api/targets/win").await.ok()["snapshots"],
        json!([])
    );
    let log = lab.log();
    assert_eq!(log.matches(restore).count(), 2, "{log}");
    ensure_up(&state, "win").await.unwrap().unwrap();
    assert_eq!(lab.log().matches(take).count(), 2);
    maybe_down(&state, "win").await.unwrap().unwrap();

    // A permanent target: no up; the snapshot only when it runs, one time.
    lab.td
        .patch("/api/targets/win", json!({"ephemeral": false}))
        .await
        .ok();
    assert!(ensure_up(&state, "win").await.unwrap().is_none());
    assert_eq!(lab.log().matches(take).count(), 2, "stopped: no snapshot");
    lab.td.post_empty("/api/targets/win/up").await.ok();
    assert!(ensure_up(&state, "win").await.unwrap().is_none());
    assert!(ensure_up(&state, "win").await.unwrap().is_none());
    let log = lab.log();
    assert_eq!(log.matches(take).count(), 3, "{log}");
    assert!(maybe_down(&state, "win").await.unwrap().is_none());
    assert_eq!(lab.log().matches(restore).count(), 2);

    // A docker target has no snapshots: the flag does nothing.
    lab.target(json!({"name": "dbox", "kind": "docker", "host": "lab1",
                      "image": "alpine:3", "ephemeral": true, "keep": true,
                      "snapshot_before_task": true}))
        .await;
    ensure_up(&state, "dbox").await.unwrap().unwrap();
    assert!(maybe_down(&state, "dbox").await.unwrap().is_none());
    assert_eq!(
        lab.td.get("/api/targets/dbox").await.ok()["snapshots"],
        json!([])
    );
    let log = lab.log();
    assert!(
        !log.contains("docker commit") && !log.contains("docker checkpoint"),
        "{log}"
    );
    let a = lab.td.get("/api/audit?action=target.snapshot").await;
    assert!(
        a.ok()
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["target"] == "win"),
        "no snapshot row for dbox"
    );
}

/// A permanent Docker target keeps its container over `down` and `up`:
/// `down` stops it, and `up` starts the same container again (`docker
/// start`), so the files in it stay. An ephemeral target starts fresh.
#[tokio::test]
async fn a_permanent_docker_target_restarts_its_container_on_up() {
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    lab.target(json!({"name": "dbox", "kind": "docker", "host": "lab1",
                      "image": "alpine:3"}))
        .await;
    assert_eq!(
        lab.td.post_empty("/api/targets/dbox/up").await.ok()["state"],
        "running"
    );
    assert_eq!(
        lab.td.post_empty("/api/targets/dbox/down").await.ok()["state"],
        "stopped"
    );
    assert_eq!(
        lab.td.post_empty("/api/targets/dbox/status").await.ok()["state"],
        "stopped"
    );
    assert_eq!(
        lab.td.post_empty("/api/targets/dbox/up").await.ok()["state"],
        "running"
    );
    let log = lab.log();
    assert!(log.contains("docker stop -t 10 terrarium-dbox"), "{log}");
    assert!(log.contains("docker start terrarium-dbox"), "{log}");
    assert_eq!(
        log.matches("docker run ").count(),
        1,
        "one run only:\n{log}"
    );
    assert!(!log.contains("docker rm"), "{log}");

    // The same target as ephemeral with keep: down stops, up makes a new
    // one. A running target does not change `ephemeral`: down first.
    lab.td
        .patch("/api/targets/dbox", json!({"ephemeral": true}))
        .await
        .err(409, "conflict");
    assert_eq!(
        lab.td.post_empty("/api/targets/dbox/down").await.ok()["state"],
        "stopped"
    );
    let r = lab
        .td
        .patch(
            "/api/targets/dbox",
            json!({"ephemeral": true, "keep": true}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(
        lab.td.post_empty("/api/targets/dbox/up").await.ok()["state"],
        "running"
    );
    assert_eq!(
        lab.td.post_empty("/api/targets/dbox/down").await.ok()["state"],
        "stopped"
    );
    let log = lab.log();
    assert!(log.contains("docker rm -f -v terrarium-dbox"), "{log}");
    assert_eq!(log.matches("docker run ").count(), 2, "{log}");
}

/// `PATCH /api/targets/{name}` (`admin target edit`) on a running target
/// changes only `keep` and `snapshot_before_task`; the other fields wait
/// until the target is down. A field given with its current value is no
/// change.
#[tokio::test]
async fn a_running_target_takes_only_keep_and_snapshot_changes() {
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    lab.target(json!({"name": "dbox", "kind": "docker", "host": "lab1",
                      "image": "alpine:3"}))
        .await;
    lab.td.post_empty("/api/targets/dbox/up").await.ok();
    let r = lab
        .td
        .patch(
            "/api/targets/dbox",
            json!({"keep": true, "snapshot_before_task": true, "image": "alpine:3"}),
        )
        .await;
    assert_eq!(r.ok()["keep"], true);
    assert_eq!(r.body["snapshot_before_task"], true);
    let r = lab
        .td
        .patch(
            "/api/targets/dbox",
            json!({"image": "python:3", "keep": false}),
        )
        .await;
    assert_eq!(r.status, 409, "{}", r.body);
    let msg = r.body["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("is running") && msg.contains("`image`"),
        "{msg}"
    );
    assert_eq!(r.body["error"]["details"]["fields"], json!(["image"]));
    let t = lab.td.get("/api/targets/dbox").await;
    assert_eq!(t.ok()["image"], "alpine:3");
    assert_eq!(t.body["keep"], true, "a refused PATCH changes nothing");
    lab.td.post_empty("/api/targets/dbox/down").await.ok();
    let r = lab
        .td
        .patch("/api/targets/dbox", json!({"image": "python:3"}))
        .await;
    assert_eq!(r.ok()["image"], "python:3");
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
    // An API target: create, patch, show in the project, delete. A
    // project target gets the project prefix.
    let r = lab
        .td
        .post(
            "/api/targets",
            json!({"name": "box", "kind": "host", "host": "lab1", "project": "terra"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["name"], "terra-box");
    assert_eq!(r.body["scope"], "project");
    lab.td
        .post(
            "/api/targets",
            json!({"name": "terra-box", "kind": "host", "host": "x"}),
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
        .patch("/api/targets/terra-box", json!({"host": "lab2"}))
        .await;
    assert_eq!(r.ok()["host"], "lab2");
    let listed = lab.td.get("/api/projects/terra/targets").await;
    let names: Vec<(String, String)> = listed
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["name"].as_str().unwrap().to_owned(),
                t["scope"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        names,
        [
            ("buildbox".to_owned(), "project".to_owned()),
            ("terra-box".to_owned(), "project".to_owned())
        ]
    );
    assert_eq!(lab.td.delete("/api/targets/terra-box").await.status, 204);
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

// ------------------------------------------------- TLS, rotation, and enrol
// (security review F2, docs/host-protocol.md "TLS" and "Tokens")

/// A TLS dial-in listener on loopback with a fresh self-signed certificate.
/// Returns its address and the certificate's SHA-256.
async fn tls_listener(lab: &Lab) -> (SocketAddr, String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let ss = terrarium_host::tls::self_signed(&["localhost".into(), "127.0.0.1".into()]).unwrap();
    let (cert, key) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::write(&cert, &ss.cert_pem).unwrap();
    std::fs::write(&key, &ss.key_pem).unwrap();
    let listen = terrariumd::hosts::HostListen {
        addr: "127.0.0.1:0".into(),
        tls: Some((cert, key)),
    };
    let (_task, addr) = listen.serve(lab.state()).await.unwrap();
    (addr, ss.fingerprint, dir)
}

fn pinned_client(lab: &Lab, addr: SocketAddr, name: &str, token: &str, pin: &str) -> Client {
    let url = format!("wss://localhost:{}/hosts/connect", addr.port());
    let t = terrarium_host::tls::transport(
        &url,
        &terrarium_host::tls::TlsSettings {
            server_fingerprint_sha256: Some(pin),
            ..Default::default()
        },
    )
    .unwrap();
    let mut o = ClientOptions::new(t.url, token, name);
    o.tls = t.tls;
    o.backoff_min = Duration::from_millis(50);
    o.backoff_max = Duration::from_millis(200);
    o.handshake_timeout = Duration::from_secs(5);
    o.docker_bin = lab.bins.path().join("docker").display().to_string();
    o.vmlab_bin = lab.bins.path().join("vmlab").display().to_string();
    Client::new(o)
}

#[tokio::test]
async fn tls_link_with_a_pinned_certificate() {
    let lab = Lab::start().await;
    let (addr, fp, _dir) = tls_listener(&lab).await;
    let token = lab.register("lab1").await;
    let task = tokio::spawn(pinned_client(&lab, addr, "lab1", &token, &fp).run());
    lab.wait_online("lab1", true).await;
    // An exec over the TLS link works end to end.
    lab.target(json!({"name": "box", "kind": "host", "host": "lab1"}))
        .await;
    let sid = exec(
        &lab.td,
        "box",
        json!({"cmd": ["sh", "-c", "echo over-tls"]}),
    )
    .await;
    let d = drain(&lab.td, "box", &sid).await;
    assert_eq!(String::from_utf8_lossy(&d.stdout), "over-tls\n");
    // The daemon records where the token came from.
    let h = lab.td.get("/api/hosts/lab1").await;
    let h = h.ok();
    assert!(h["first_used_at"].is_string(), "{h}");
    assert!(
        h["last_used_from"]
            .as_str()
            .is_some_and(|f| f.starts_with("127.0.0.1:")),
        "{h}"
    );
    task.abort();

    // A wrong pin fails the TLS handshake; no token is sent.
    let token2 = lab.register("lab2").await;
    let wrong = "00".repeat(32);
    let err = pinned_client(&lab, addr, "lab2", &token2, &wrong)
        .connect_once()
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains(&fp),
        "the error names the seen fingerprint: {err}"
    );
    assert!(!lab.state().executor.hosts.is_online("lab2"));

    // Plaintext to the TLS listener does not get through.
    let mut o = ClientOptions::new(format!("ws://{addr}/hosts/connect"), &token2, "lab2");
    o.handshake_timeout = Duration::from_secs(3);
    assert!(Client::new(o).connect_once().await.is_err());
    assert!(!lab.state().executor.hosts.is_online("lab2"));
}

#[tokio::test]
async fn plaintext_to_a_remote_host_needs_allow_insecure() {
    // No network: the client refuses before it dials.
    let mut o = ClientOptions::new("ws://terrarium.invalid:8082/hosts/connect", "t", "lab1");
    o.handshake_timeout = Duration::from_secs(3);
    let err = Client::new(o.clone()).connect_once().await.unwrap_err();
    assert!(
        matches!(err, terrarium_host::client::ClientError::Insecure(_)),
        "{err}"
    );
    assert!(err.to_string().contains("allow_insecure"), "{err}");
    // `run` stops at once in place of retrying for ever.
    tokio::time::timeout(Duration::from_secs(5), Client::new(o.clone()).run())
        .await
        .expect("run returns on an insecure transport");
    // With allow_insecure the client dials (and fails on the name).
    o.allow_insecure = true;
    let err = Client::new(o).connect_once().await.unwrap_err();
    assert!(
        !matches!(err, terrarium_host::client::ClientError::Insecure(_)),
        "{err}"
    );
    // The config path gives the same answer.
    let cfg = terrarium_host::Config {
        daemon_url: Some("ws://10.1.2.3:8082".into()),
        ..Default::default()
    };
    assert!(cfg.transport().is_err());
}

#[test]
fn host_listen_defaults_follow_tls() {
    use terrariumd::hosts::HostListen;
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |k: &str| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_owned())
        }
    };
    let plain = HostListen::from_vars(env(&[])).unwrap().unwrap();
    assert_eq!(plain.addr, "127.0.0.1:8082");
    assert!(plain.tls.is_none());
    let tls = HostListen::from_vars(env(&[
        ("TERRARIUM_HOST_TLS_CERT", "/tls/c.pem"),
        ("TERRARIUM_HOST_TLS_KEY", "/tls/k.pem"),
    ]))
    .unwrap()
    .unwrap();
    assert_eq!(tls.addr, "0.0.0.0:8082");
    assert!(tls.tls.is_some());
    // Empty values (compose passes `${X:-}`) count as unset.
    let empty = HostListen::from_vars(env(&[
        ("TERRARIUM_HOST_TLS_CERT", ""),
        ("TERRARIUM_HOST_TLS_KEY", ""),
    ]))
    .unwrap()
    .unwrap();
    assert_eq!(empty.addr, "127.0.0.1:8082");
    let explicit =
        HostListen::from_vars(env(&[("TERRARIUM_HOST_LISTEN", "0.0.0.0:9000")])).unwrap();
    assert_eq!(explicit.unwrap().addr, "0.0.0.0:9000");
    let auto = HostListen::from_vars(env(&[("TERRARIUM_HOST_LISTEN", "auto")])).unwrap();
    assert_eq!(auto.unwrap().addr, "127.0.0.1:8082");
    assert!(
        HostListen::from_vars(env(&[("TERRARIUM_HOST_LISTEN", "")]))
            .unwrap()
            .is_none()
    );
    assert!(HostListen::from_vars(env(&[("TERRARIUM_HOST_TLS_CERT", "/c.pem")])).is_err());
}

async fn dial_status(lab: &Lab, token: &str) -> u16 {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = format!("ws://{}/hosts/connect", lab.addr)
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    match tokio_tungstenite::connect_async(req).await {
        Ok(_) => 101,
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => resp.status().as_u16(),
        Err(other) => panic!("dial: {other}"),
    }
}

#[tokio::test]
async fn rotation_keeps_the_live_link_and_retires_the_old_token() {
    let lab = Lab::start().await;
    let old = lab.register("lab1").await;
    let task = tokio::spawn(lab.client("lab1", &old).run());
    lab.wait_online("lab1", true).await;
    let first_used = lab.td.get("/api/hosts/lab1").await.body["first_used_at"].clone();
    assert!(first_used.is_string());

    // Only the operator rotates.
    lab.td.project("ops").await;
    let sys = lab
        .td
        .agent("ops", "lead", json!({"roles": ["sysadmin"]}))
        .await;
    let sys = sys["id"].as_str().unwrap().to_owned();
    lab.td
        .post_as(&sys, "/api/hosts/lab1/rotate", json!({}))
        .await
        .err(403, "forbidden");

    // An empty body is a plain rotation.
    let r = lab
        .td
        .req(hyper::Method::POST, "/api/hosts/lab1/rotate", None, None)
        .await;
    let r = r.ok();
    let new = r["token"].as_str().unwrap().to_owned();
    assert!(new.starts_with("thk_"));
    assert_ne!(new, old);
    assert_eq!(r["token_kind"], "host");
    assert!(r["rotated_at"].is_string());
    // The token is shown once.
    assert!(
        lab.td
            .get("/api/hosts/lab1")
            .await
            .body
            .get("token")
            .is_none()
    );
    let audit = lab.td.get("/api/audit?action=host.rotate").await;
    assert_eq!(audit.ok().as_array().unwrap().len(), 1);

    // The live link stays up on the old token...
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(lab.state().executor.hosts.is_online("lab1"));
    // ...but a new dial with it gets 401, so after the link ends the host
    // stays offline.
    assert_eq!(dial_status(&lab, &old).await, 401);
    assert!(lab.state().executor.hosts.drop_link("lab1"));
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!lab.state().executor.hosts.is_online("lab1"));
    task.abort();

    // The new token works.
    let task = tokio::spawn(lab.client("lab1", &new).run());
    lab.wait_online("lab1", true).await;
    task.abort();
}

#[tokio::test]
async fn an_enrol_token_is_swapped_for_a_stored_long_lived_token() {
    let lab = Lab::start().await;
    let r = lab
        .td
        .post("/api/hosts", json!({"name": "lab1", "enrol": true}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let enrol = r.body["token"].as_str().unwrap().to_owned();
    assert!(enrol.starts_with("the_"), "{enrol}");
    assert_eq!(r.body["token_kind"], "enrol");
    assert!(r.body["token_expires_at"].is_string());
    assert!(r.body["enrol_expires_at"].is_string());

    // The host agent's token_file holds the enrol token.
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("token");
    std::fs::write(&token_file, format!("{enrol}\n")).unwrap();
    let cfg = terrarium_host::Config {
        token_file: Some(token_file.clone()),
        ..Default::default()
    };
    let read = cfg.resolve_token().unwrap();
    assert_eq!(read, enrol);
    let mut o = ClientOptions::new(format!("ws://{}/hosts/connect", lab.addr), read, "lab1");
    o.token_file = Some(token_file.clone());
    o.backoff_min = Duration::from_millis(50);
    o.backoff_max = Duration::from_millis(200);
    let c = Client::new(o);
    let task = tokio::spawn(c.clone().run());
    lab.wait_online("lab1", true).await;

    // The long-lived token is in the file now, mode 0600, and in use.
    let stored = std::fs::read_to_string(&token_file).unwrap();
    let stored = stored.trim().to_owned();
    assert!(stored.starts_with("thk_"), "{stored}");
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&token_file).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(c.bearer(), stored);
    let h = lab.td.get("/api/hosts/lab1").await;
    assert!(h.body["enrol_expires_at"].is_null(), "{}", h.body);
    let ev = events(&lab.td, "host.connected").await;
    assert_eq!(ev[0]["data"]["enrolled"], true);
    let audit = lab.td.get("/api/audit?action=host.enrol").await;
    assert_eq!(audit.ok().as_array().unwrap().len(), 1);

    // The enrol token is one time.
    assert_eq!(dial_status(&lab, &enrol).await, 401);

    // A reconnect uses the stored token.
    assert!(lab.state().executor.hosts.drop_link("lab1"));
    for _ in 0..300 {
        if events(&lab.td, "host.connected").await.len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let ev = events(&lab.td, "host.connected").await;
    assert_eq!(ev.len(), 2, "the host came back with the stored token");
    assert_eq!(ev[1]["data"]["enrolled"], false);
    task.abort();

    // A rotation can hand out a new enrol token for a host that exists.
    let r = lab
        .td
        .post("/api/hosts/lab1/rotate", json!({"enrol": true}))
        .await;
    let r = r.ok();
    let enrol2 = r["token"].as_str().unwrap().to_owned();
    assert!(enrol2.starts_with("the_"));
    // The long-lived token stops working for new dials.
    assert_eq!(dial_status(&lab, &stored).await, 401);
    // An enrol token past its time gets 401.
    lab.state()
        .db
        .write("test", |t| {
            t.execute(
                "UPDATE hosts SET enrol_expires_at = '2000-01-01T00:00:00.000Z'",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(dial_status(&lab, &enrol2).await, 401);
}

/// The ULID of an agent made with `TestDaemon::agent`.
fn id_of(a: &Value) -> String {
    a["id"].as_str().unwrap().to_owned()
}

/// The last audit row of `action`.
async fn last_audit(td: &TestDaemon, action: &str) -> Value {
    let a = td.get(&format!("/api/audit?action={action}")).await;
    a.ok()
        .as_array()
        .unwrap()
        .last()
        .unwrap_or_else(|| panic!("no audit row {action}"))
        .clone()
}

/// Target leases (docs/api.md §17.7): a lead grants a project target to a
/// worker, the worker uses it, and a revoke ends the worker's live session
/// with `lease_revoked`. The next exec is `403` and names the lease.
#[tokio::test]
async fn a_lease_grants_and_a_revoke_ends_the_sessions() {
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    lab.td.project("terra").await;
    let lead = id_of(
        &lab.td
            .agent("terra", "lead", json!({"roles": ["project_admin"]}))
            .await,
    );
    let w = id_of(&lab.td.agent("terra", "worker", json!({})).await);
    lab.target(json!({"name": "terra-box", "kind": "host", "host": "lab1", "project": "terra"}))
        .await;
    let body = json!({"cmd": ["sleep", "30"]});
    let r = lab
        .td
        .post_as(&w, "/api/targets/terra-box/exec", body.clone())
        .await;
    r.err(403, "forbidden");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no lease"),
        "{}",
        r.body
    );

    let g = lab
        .td
        .post_as(
            &lead,
            "/api/targets/terra-box/leases",
            json!({"agent": "worker", "reason": "TER-1"}),
        )
        .await;
    assert_eq!(g.status, 201, "{}", g.body);
    assert_eq!(g.body["state"], "active");
    assert_eq!(g.body["agent_name"], "terra/worker");
    assert_eq!(g.body["expires_at"], Value::Null);
    assert_eq!(g.body["granted_by"], format!("agent:{lead}"));
    // `targets ls` for the worker shows the lease.
    let mine = lab.td.get_as(&w, "/api/targets?usable=true").await;
    let mine = mine.ok().as_array().unwrap().clone();
    assert_eq!(mine.len(), 1, "{mine:?}");
    assert_eq!(mine[0]["access"], "lease");
    assert_eq!(mine[0]["lease"]["exclusive"], false);
    let held = lab.td.get("/api/agents/terra%2Fworker/leases").await;
    assert_eq!(held.ok().as_array().unwrap().len(), 1);

    let r = lab
        .td
        .post_as(&w, "/api/targets/terra-box/exec", body.clone())
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let sid = r.body["session_id"].as_str().unwrap().to_owned();
    let r = lab
        .td
        .req(
            hyper::Method::DELETE,
            "/api/targets/terra-box/leases/worker",
            None,
            Some(&lead),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["state"], "revoked");
    let d = drain_as(&lab.td, Some(&w), "terra-box", &sid).await;
    assert_eq!(d.end["type"], "error", "{}", d.end);
    assert_eq!(d.end["code"], "lease_revoked", "{}", d.end);
    let r = lab
        .td
        .post_as(&w, "/api/targets/terra-box/exec", body)
        .await;
    r.err(403, "forbidden");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("your lease on terra-box ended"),
        "{}",
        r.body
    );
    // A second revoke finds no lease.
    lab.td
        .delete("/api/targets/terra-box/leases/worker")
        .await
        .err(404, "not_found");
    let row = last_audit(&lab.td, "target.grant").await;
    assert_eq!(row["actor"], format!("agent:{lead}"));
    assert_eq!(row["detail"]["reason"], "TER-1");
    let row = last_audit(&lab.td, "target.revoke").await;
    assert_eq!(row["target"], "terra-box");
    let all = lab.td.get("/api/targets/terra-box/leases?all=true").await;
    assert_eq!(all.ok().as_array().unwrap().len(), 1);
    assert_eq!(
        lab.td.get("/api/targets/terra-box/leases").await.body,
        json!([])
    );
    let ev = events(&lab.td, "lease").await;
    let changes: Vec<&str> = ev
        .iter()
        .map(|e| e["data"]["change"].as_str().unwrap())
        .collect();
    assert_eq!(changes, ["granted", "ended"], "{ev:#?}");
}

/// A lease with a short `until` expires: the timer ends the session with
/// `lease_revoked`, writes `target.lease_expired` as `system`, and the
/// next exec is `403`.
#[tokio::test]
async fn an_expired_lease_ends_the_sessions() {
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    lab.td.project("terra").await;
    let w = id_of(&lab.td.agent("terra", "worker", json!({})).await);
    lab.target(json!({"name": "box", "kind": "host", "host": "lab1"}))
        .await;
    let g = lab
        .td
        .post(
            "/api/targets/box/leases",
            json!({"agent": "terra/worker", "until": "2s"}),
        )
        .await;
    assert_eq!(g.status, 201, "{}", g.body);
    assert!(g.body["expires_at"].is_string(), "{}", g.body);
    let r = lab
        .td
        .post_as(&w, "/api/targets/box/exec", json!({"cmd": ["sleep", "30"]}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let sid = r.body["session_id"].as_str().unwrap().to_owned();
    let started = std::time::Instant::now();
    let d = drain_as(&lab.td, Some(&w), "box", &sid).await;
    assert_eq!(d.end["code"], "lease_revoked", "{}", d.end);
    assert!(started.elapsed() < Duration::from_secs(15));
    let row = last_audit(&lab.td, "target.lease_expired").await;
    assert_eq!(row["actor"], "system");
    assert_eq!(row["target"], "box");
    lab.td
        .post_as(&w, "/api/targets/box/exec", json!({"cmd": ["true"]}))
        .await
        .err(403, "forbidden");
    let all = lab
        .td
        .get("/api/agents/terra%2Fworker/leases?all=true")
        .await;
    assert_eq!(all.ok()[0]["state"], "expired");
    assert_eq!(all.ok()[0]["revoked_by"], "system");
}

/// An exclusive lease refuses a second lease and names the holder. A
/// project_admin leases its project's targets to its project's agents; a
/// global target, or an agent of another project, needs a sysadmin or the
/// operator. A member cannot lease.
#[tokio::test]
async fn exclusive_leases_and_who_may_grant() {
    let lab = Lab::start().await;
    lab.td.project("terra").await;
    lab.td.project("ops").await;
    let lead = id_of(
        &lab.td
            .agent("terra", "lead", json!({"roles": ["project_admin"]}))
            .await,
    );
    let w1 = id_of(&lab.td.agent("terra", "w1", json!({})).await);
    lab.td.agent("terra", "w2", json!({})).await;
    lab.td.agent("ops", "o1", json!({})).await;
    let sys = id_of(
        &lab.td
            .agent("ops", "sys", json!({"roles": ["sysadmin"]}))
            .await,
    );
    lab.target(json!({"name": "terra-vm", "kind": "host", "host": "lab1", "project": "terra"}))
        .await;
    lab.target(json!({"name": "g", "kind": "host", "host": "lab1"}))
        .await;
    let grant = |who: String, target: &'static str, body: Value| {
        let td = &lab.td;
        async move {
            td.post_as(&who, &format!("/api/targets/{target}/leases"), body)
                .await
        }
    };
    let r = grant(
        lead.clone(),
        "terra-vm",
        json!({"agent": "w1", "until": "30m", "exclusive": true}),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["exclusive"], true);
    let until = r.body["expires_at"].as_str().unwrap().to_owned();
    let r = grant(lead.clone(), "terra-vm", json!({"agent": "w2"})).await;
    r.err(409, "lease_held");
    let msg = r.body["error"]["message"].as_str().unwrap();
    assert!(msg.contains("terra/w1") && msg.contains(&until), "{msg}");
    assert_eq!(r.body["error"]["details"]["holder"], "terra/w1");
    assert_eq!(r.body["error"]["details"]["expires_at"], until.as_str());
    // The rights.
    let r = grant(lead.clone(), "g", json!({"agent": "w1"})).await;
    r.err(403, "forbidden");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("sysadmin"),
        "{}",
        r.body
    );
    grant(lead.clone(), "terra-vm", json!({"agent": "ops/o1"}))
        .await
        .err(403, "forbidden");
    grant(w1.clone(), "terra-vm", json!({"agent": "w2"}))
        .await
        .err(403, "forbidden");
    let r = grant(sys.clone(), "g", json!({"agent": "terra/w2"})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let r = lab
        .td
        .post("/api/targets/g/leases", json!({"agent": "ops/o1"}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    // A project target goes only to an agent of its project.
    lab.td
        .post("/api/targets/terra-vm/leases", json!({"agent": "ops/o1"}))
        .await
        .err(422, "invalid");
    // Extend, then a holder gives its own lease back.
    let r = lab
        .td
        .patch_as(
            &lead,
            "/api/targets/terra-vm/leases/w1",
            json!({"until": "2h"}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body["expires_at"].as_str().unwrap() > until.as_str());
    assert_eq!(
        last_audit(&lab.td, "target.lease_extend").await["actor"],
        format!("agent:{lead}")
    );
    let r = lab
        .td
        .req(
            hyper::Method::DELETE,
            "/api/targets/terra-vm/leases/w1",
            None,
            Some(&w1),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let r = grant(lead, "terra-vm", json!({"agent": "w2", "exclusive": true})).await;
    assert_eq!(r.status, 201, "{}", r.body);
}

/// A grant to a running agent brings an ephemeral target up (reason
/// `lease_grant`); the revoke that leaves no running holder brings it down
/// (reason `lease_end`). A grant to a stopped agent runs no verb.
#[tokio::test]
async fn a_grant_to_a_running_agent_brings_an_ephemeral_target_up() {
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    lab.td.project("terra").await;
    let w = id_of(&lab.td.agent("terra", "worker", json!({})).await);
    lab.td.agent("terra", "idle", json!({})).await;
    lab.target(
        json!({"name": "terra-dbox", "kind": "docker", "host": "lab1", "project": "terra",
                      "image": "alpine:3", "ephemeral": true}),
    )
    .await;
    // A stopped agent: the grant runs no verb.
    let r = lab
        .td
        .post(
            "/api/targets/terra-dbox/leases",
            json!({"agent": "terra/idle"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert!(r.body.get("lifecycle").is_none(), "{}", r.body);
    assert!(!lab.log().contains("docker run"), "{}", lab.log());
    lab.td
        .delete("/api/targets/terra-dbox/leases/terra%2Fidle")
        .await
        .ok();
    assert!(!lab.log().contains("docker rm"), "{}", lab.log());

    lab.td
        .post_empty(&format!("/api/agents/{w}/start"))
        .await
        .ok();
    lab.td.wait_status(&w, "idle").await;
    let r = lab
        .td
        .post(
            "/api/targets/terra-dbox/leases",
            json!({"agent": "terra/worker", "until": "1h"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["lifecycle"]["state"], "running", "{}", r.body);
    assert!(
        lab.log()
            .contains("docker run -d --init --name terrarium-terra-dbox"),
        "{}",
        lab.log()
    );
    let row = last_audit(&lab.td, "target.up").await;
    assert_eq!(row["actor"], "system");
    assert_eq!(row["detail"]["reason"], "lease_grant");
    let r = lab
        .td
        .delete("/api/targets/terra-dbox/leases/terra%2Fworker")
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(
        lab.log().contains("docker rm -f -v terrarium-terra-dbox"),
        "{}",
        lab.log()
    );
    let row = last_audit(&lab.td, "target.destroy").await;
    assert_eq!(row["detail"]["reason"], "lease_end");
    assert_eq!(
        lab.td.get("/api/targets/terra-dbox").await.ok()["state"],
        "absent"
    );
}

/// A running agent that holds the target in its policy keeps it up when
/// another agent's lease ends. The lease to a running agent attaches its
/// workspace only when it can be attached safely. Even with no live exec
/// session, joining and leaving must not replace the running container:
/// it may still contain background processes (TER-203).
#[tokio::test]
async fn a_lease_end_leaves_a_target_that_a_policy_holder_uses() {
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    lab.td.project("terra").await;
    let owner = id_of(
        &lab.td
            .agent(
                "terra",
                "owner",
                json!({"policy": {"targets": ["terra-dbox"]}}),
            )
            .await,
    );
    let w = id_of(&lab.td.agent("terra", "worker", json!({})).await);
    lab.target(
        json!({"name": "terra-dbox", "kind": "docker", "host": "lab1", "project": "terra",
                      "image": "alpine:3", "ephemeral": true}),
    )
    .await;
    for a in [&owner, &w] {
        lab.td
            .post_empty(&format!("/api/agents/{a}/start"))
            .await
            .ok();
        lab.td.wait_status(a, "idle").await;
    }
    lab.td.post_empty("/api/targets/terra-dbox/up").await.ok();
    let t = lab.td.get("/api/targets/terra-dbox").await;
    assert_eq!(
        t.ok()["runtime"]["attached"],
        json!(["terra/owner"]),
        "{}",
        t.body
    );
    let r = lab
        .td
        .post(
            "/api/targets/terra-dbox/leases",
            json!({"agent": "terra/worker"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["lifecycle"]["state"], "running", "{}", r.body);
    assert!(
        r.body["lifecycle_error"]
            .as_str()
            .unwrap()
            .contains("must not replace a running container"),
        "{}",
        r.body
    );
    let log = lab.log();
    let last_run = log.lines().rfind(|l| l.starts_with("docker run")).unwrap();
    assert!(last_run.contains(":/work/terra/owner"), "{last_run}");
    assert!(!last_run.contains(":/work/terra/worker"), "{last_run}");
    assert!(
        r.body["lifecycle"]["attach_error"]
            .as_str()
            .unwrap()
            .contains("must not replace a running container"),
        "{}",
        r.body
    );
    assert_eq!(
        last_audit(&lab.td, "target.attach").await["detail"]["reason"],
        "lease_grant"
    );
    lab.td
        .delete("/api/targets/terra-dbox/leases/terra%2Fworker")
        .await
        .ok();
    let log = lab.log();
    let last_run = log.lines().rfind(|l| l.starts_with("docker run")).unwrap();
    assert!(last_run.contains(":/work/terra/owner"), "{last_run}");
    assert!(!last_run.contains("worker"), "{last_run}");
    assert_eq!(log.matches("docker run ").count(), 1, "{log}");
    assert!(!log.contains("docker rm "), "{log}");
    assert!(!log.contains("docker stop "), "{log}");
    let t = lab.td.get("/api/targets/terra-dbox").await;
    assert_eq!(t.ok()["state"], "running");
    assert_eq!(t.body["runtime"]["attached"], json!(["terra/owner"]));
    // No destroy or replacement for a lease end.
    assert!(!lab.log().contains("target.destroy"));
}

/// The grant pointer (docs/api.md §17.7) comes after the lifecycle step of
/// the grant: after the recorded `vmlab up` and the `terrarium-clean`
/// snapshot, and it names the state. A permanent target that is stopped
/// says so. An extend gives no pointer.
#[tokio::test]
async fn the_grant_pointer_follows_the_lifecycle_and_names_the_state() {
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    lab.td.project("terra").await;
    let w = id_of(&lab.td.agent("terra", "worker", json!({})).await);
    let labdir = tempfile::tempdir().unwrap();
    std::fs::write(labdir.path().join("vmlab.wcl"), "lab \"win\" {}\n").unwrap();
    lab.target(json!({"name": "terra-win", "kind": "vmlab", "host": "lab1",
                      "project": "terra", "lab": labdir.path(), "machine": "win11",
                      "ephemeral": true, "snapshot_before_task": true}))
        .await;
    lab.target(
        json!({"name": "terra-pbox", "kind": "docker", "host": "lab1",
                      "project": "terra", "image": "alpine:3"}),
    )
    .await;
    lab.td.post_empty("/api/targets/terra-pbox/up").await.ok();
    lab.td.post_empty("/api/targets/terra-pbox/down").await.ok();
    start_agent(&lab, &w).await;
    // `vmlab up` takes a second.
    std::fs::write(lab.bins.path().join("slow"), "").unwrap();
    let agent: terrarium_core::AgentId = w.parse().unwrap();
    let sup = lab.td.sup.clone();
    let pointers = |n: usize| {
        let sup = sup.clone();
        async move {
            for _ in 0..200 {
                let p = sup.lease_pointers(agent);
                if p.len() >= n {
                    return p;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("no pointer {n}");
        }
    };
    let r = lab
        .td
        .post(
            "/api/targets/terra-win/leases",
            json!({"agent": "terra/worker", "until": "1h"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let until = r.body["expires_at"].as_str().unwrap().to_owned();
    let p = pointers(1).await;
    assert_eq!(
        p[0].0,
        format!(
            "You now have access to target terra-win (running) until {until}. \
             Run `terrarium targets ls`. {}",
            // A vmlab target with a machine has a screen (docs/api.md §17.10).
            terrariumd::leases::display_hint("terra-win", false)
        )
    );
    let t = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap();
    let pointer_at = t(&p[0].1);
    let up = last_audit(&lab.td, "target.up").await;
    let snap = last_audit(&lab.td, "target.snapshot").await;
    assert_eq!(up["detail"]["reason"], "lease_grant");
    assert_eq!(snap["detail"]["reason"], "snapshot_before_task");
    assert!(t(up["at"].as_str().unwrap()) <= pointer_at, "{up} {p:?}");
    assert!(
        t(snap["at"].as_str().unwrap()) <= pointer_at,
        "{snap} {p:?}"
    );
    let log = lab.log();
    assert!(log.contains("vmlab up win11"), "{log}");
    assert!(
        log.contains("vmlab snapshot create --vm win11 terrarium-clean"),
        "{log}"
    );

    // A permanent target that is stopped: no verb, and the pointer says so.
    let r = lab
        .td
        .post(
            "/api/targets/terra-pbox/leases",
            json!({"agent": "terra/worker"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert!(r.body.get("lifecycle").is_none(), "{}", r.body);
    let p = pointers(2).await;
    assert_eq!(
        p[1].0,
        "You now have access to target terra-pbox (stopped: ask your lead to bring it up) \
         until revoked. Run `terrarium targets ls`."
    );
    // An extend: no pointer.
    let r = lab
        .td
        .patch(
            "/api/targets/terra-win/leases/terra%2Fworker",
            json!({"until": "2h"}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(lab.td.sup.lease_pointers(agent).len(), 2);
}

/// When a host agent connects, the daemon reconciles the stored state of
/// its docker and vmlab targets with what runs there (a `status` of each):
/// a container that is gone is `absent`, a lab that is not running (not
/// in `vmlab lab list`) of an ephemeral target is `absent` with no
/// snapshots. Each change writes `target.reconcile` and a `target` event;
/// a target that matches writes nothing. A later `status` does the same.
#[tokio::test]
async fn a_host_connect_reconciles_the_target_states() {
    let lab = Lab::start().await;
    let token = lab.register("lab1").await;
    let labdir = tempfile::tempdir().unwrap();
    std::fs::write(labdir.path().join("vmlab.wcl"), "lab \"win\" {}\n").unwrap();
    for (name, image) in [
        ("dgone", true),
        ("drun", true),
        ("dstop", false),
        ("dsame", true),
    ] {
        let mut def = json!({"name": name, "kind": "docker", "host": "lab1",
                             "image": "alpine:3"});
        if !image {
            def["ephemeral"] = false.into();
        }
        lab.target(def).await;
    }
    lab.target(json!({"name": "vgone", "kind": "vmlab", "host": "lab1",
                      "lab": labdir.path(), "machine": "win11", "ephemeral": true}))
        .await;
    lab.target(json!({"name": "vkept", "kind": "vmlab", "host": "lab1",
                      "lab": labdir.path(), "machine": "win11"}))
        .await;
    // What the store says, and what runs.
    let st = lab.state();
    st.db
        .write("system", |t| {
            use terrariumd::store::targets::set_state;
            set_state(t, "dgone", "running", None)?;
            set_state(t, "drun", "stopped", None)?;
            set_state(t, "dstop", "running", None)?;
            set_state(t, "dsame", "running", None)?;
            set_state(t, "vgone", "stopped", Some("terrarium-clean"))?;
            set_state(t, "vkept", "stopped", Some("clean"))?;
            Ok(())
        })
        .await
        .unwrap();
    let docker = |c: &str, s: &str| {
        std::fs::write(
            lab.bins.path().join(format!("docker.state.terrarium-{c}")),
            format!("{s}\n"),
        )
        .unwrap();
    };
    docker("drun", "running");
    docker("dstop", "exited");
    docker("dsame", "running");
    std::fs::write(lab.bins.path().join("vmlab.gone"), "").unwrap();

    let _h = tokio::spawn(lab.client("lab1", &token).run());
    lab.wait_online("lab1", true).await;
    // Wait for the six `status` calls and the four changes.
    for i in 0..300 {
        let n = lab
            .td
            .get("/api/audit?action=target.reconcile")
            .await
            .ok()
            .as_array()
            .unwrap()
            .len();
        let log = lab.log();
        if n >= 4
            && log.matches("docker inspect").count() >= 4
            && log.matches("vmlab status").count() >= 2
        {
            break;
        }
        assert!(i < 299, "no reconcile:\n{log}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let state = |n: &'static str| {
        let td = &lab.td;
        async move { td.get(&format!("/api/targets/{n}")).await.ok().clone() }
    };
    assert_eq!(state("dgone").await["state"], "absent");
    assert_eq!(state("drun").await["state"], "running");
    assert_eq!(state("dstop").await["state"], "stopped");
    assert_eq!(state("dsame").await["state"], "running");
    let v = state("vgone").await;
    assert_eq!(
        (&v["state"], &v["snapshots"]),
        (&json!("absent"), &json!([]))
    );
    // A permanent lab that is not running is down with its snapshots.
    let v = state("vkept").await;
    assert_eq!(
        (&v["state"], &v["snapshots"]),
        (&json!("stopped"), &json!(["clean"]))
    );
    let rows = lab
        .td
        .get("/api/audit?action=target.reconcile")
        .await
        .ok()
        .as_array()
        .unwrap()
        .clone();
    let mut changed: Vec<&str> = rows.iter().map(|r| r["target"].as_str().unwrap()).collect();
    changed.sort_unstable();
    assert_eq!(changed, ["dgone", "drun", "dstop", "vgone"], "{rows:#?}");
    let vg = rows.iter().find(|r| r["target"] == "vgone").unwrap();
    assert_eq!(vg["actor"], "system");
    assert_eq!(vg["detail"]["from"], "stopped");
    assert_eq!(
        vg["detail"]["snapshots_cleared"],
        json!(["terrarium-clean"])
    );
    let ev = events(&lab.td, "target").await;
    assert_eq!(ev.len(), 4, "{ev:#?}");
    assert!(ev.iter().all(|e| e["data"]["change"] == "reconciled"));
    let log = lab.log();
    assert!(
        log.contains("docker inspect --type container terrarium-dsame"),
        "{log}"
    );

    // A `status` that matches writes nothing; one that differs reconciles.
    lab.td.post_empty("/api/targets/dsame/status").await.ok();
    assert_eq!(events(&lab.td, "target").await.len(), 4);
    std::fs::remove_file(lab.bins.path().join("docker.state.terrarium-dsame")).unwrap();
    let r = lab.td.post_empty("/api/targets/dsame/status").await;
    assert_eq!(r.ok()["state"], "absent");
    let ev = events(&lab.td, "target").await;
    assert_eq!(ev.len(), 5, "{ev:#?}");
    assert_eq!(ev[4]["data"]["from"], "running");
    assert_eq!(
        last_audit(&lab.td, "target.reconcile").await["target"],
        "dsame"
    );
}

// ------------------------------------------------------------ mode 1

/// The `docker run` lines of the fake's log.
fn runs(lab: &Lab) -> Vec<String> {
    lab.log()
        .lines()
        .filter(|l| l.starts_with("docker run"))
        .map(str::to_owned)
        .collect()
}

async fn start_agent(lab: &Lab, id: &str) {
    lab.td
        .post_empty(&format!("/api/agents/{id}/start"))
        .await
        .ok();
    lab.td.wait_status(id, "idle").await;
}

async fn stop_agent(lab: &Lab, id: &str) {
    lab.td
        .post_empty(&format!("/api/agents/{id}/stop"))
        .await
        .ok();
    lab.td.wait_status(id, "stopped").await;
}

/// PLAN §11 "mode 1": an agent's workspace is `<work>/<project>/<agent>`,
/// and a docker target binds the workspaces of its running holders at the
/// same path. A holder that joins a running container gets an explicit
/// attach error, even without a live exec session. Neither joining nor
/// leaving replaces the container while another holder uses it.
#[tokio::test]
async fn docker_targets_attach_the_workspaces_of_their_holders() {
    use terrariumd::executor::{on_agent_start, on_agent_stop};
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    let state = lab.state();
    lab.td.project("terra").await;
    let pol = json!({"policy": {"targets": ["terra-dbox"]}});
    let w1 = id_of(&lab.td.agent("terra", "w1", pol.clone()).await);
    let w2 = id_of(&lab.td.agent("terra", "w2", pol.clone()).await);
    let w3 = id_of(&lab.td.agent("terra", "w3", pol.clone()).await);
    let a = lab.td.get(&format!("/api/agents/{w1}")).await;
    let work = lab.td.state.with_file_name("work");
    assert_eq!(
        a.ok()["workspace"].as_str().unwrap(),
        work.join("terra/w1").display().to_string()
    );
    lab.target(
        json!({"name": "dbox", "kind": "docker", "host": "lab1", "project": "terra",
                      "image": "alpine:3", "ephemeral": true}),
    )
    .await;
    let ws = |n: &str| {
        std::fs::canonicalize(work.join("terra").join(n))
            .unwrap_or_else(|_| work.join("terra").join(n))
    };

    start_agent(&lab, &w1).await;
    assert!(
        on_agent_start(&state, &w1, &["terra-dbox".into()])
            .await
            .is_empty()
    );
    let r = runs(&lab);
    assert_eq!(r.len(), 1, "{r:?}");
    let v1 = format!("-v {}:/work/terra/w1", ws("w1").display());
    assert!(
        r[0].contains("--name terrarium-terra-dbox -e HOME") && r[0].contains(&v1),
        "{}",
        r[0]
    );
    // As the owner of the workspace, so a build's files stay the agent's.
    assert!(r[0].contains(" -u "), "{}", r[0]);
    assert!(work.join("terra/w1").is_dir());

    // A second holder joins with no live session: background processes
    // may remain, so the container must not be replaced.
    start_agent(&lab, &w2).await;
    let errs = on_agent_start(&state, &w2, &["terra-dbox".into()]).await;
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(
        errs[0]
            .1
            .message
            .contains("must not replace a running container")
    );
    let r = runs(&lab);
    assert_eq!(r.len(), 1, "{r:?}");
    assert!(!lab.log().contains("docker rm "));
    assert!(!lab.log().contains("docker stop "));
    let t = lab.td.get("/api/targets/terra-dbox").await;
    assert_eq!(t.ok()["runtime"]["attached"], json!(["terra/w1"]));

    // A live session: the third holder cannot join. The fake `sleeper`
    // runs until the test writes `sleeper.end` (at most 60 s), not for a
    // fixed time: under load the start of w3 can outlast a fixed sleep
    // (TER-118).
    let sid = exec(&lab.td, "terra-dbox", json!({"cmd": ["sleeper"]})).await;
    start_agent(&lab, &w3).await;
    let errs = on_agent_start(&state, &w3, &["terra-dbox".into()]).await;
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(
        errs[0]
            .1
            .message
            .contains("attach needs a restart of the target"),
        "{}",
        errs[0].1.message
    );
    assert!(
        errs[0]
            .1
            .message
            .contains("must not replace a running container"),
        "{}",
        errs[0].1.message
    );
    assert_eq!(runs(&lab).len(), 1, "no new container");
    std::fs::write(lab.bins.path().join("sleeper.end"), "").unwrap();
    let _ = drain(&lab.td, "terra-dbox", &sid).await;
    stop_agent(&lab, &w3).await;
    assert!(
        on_agent_stop(&state, &w3, &["terra-dbox".into()])
            .await
            .is_empty()
    );
    assert_eq!(
        runs(&lab).len(),
        1,
        "w3 was not attached: nothing to detach"
    );

    // A holder leaves: the original container remains untouched.
    stop_agent(&lab, &w2).await;
    assert!(
        on_agent_stop(&state, &w2, &["terra-dbox".into()])
            .await
            .is_empty()
    );
    let r = runs(&lab);
    assert_eq!(r.len(), 1, "{r:?}");
    assert!(!lab.log().contains("docker rm "));
    assert!(!lab.log().contains("docker stop "));
    // The last holder leaves: the ephemeral target goes.
    stop_agent(&lab, &w1).await;
    assert!(
        on_agent_stop(&state, &w1, &["terra-dbox".into()])
            .await
            .is_empty()
    );
    let t = lab.td.get("/api/targets/terra-dbox").await;
    assert_eq!(t.ok()["state"], "absent");
    assert_eq!(t.body["runtime"], json!({}));
}

/// A permanent docker target keeps its container: a holder that joins it
/// while it runs gets "attach needs a restart of the target", and no
/// container goes.
#[tokio::test]
async fn a_permanent_docker_target_refuses_a_late_attach() {
    use terrariumd::executor::on_agent_start;
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    let state = lab.state();
    lab.td.project("terra").await;
    let pol = json!({"policy": {"targets": ["terra-keep"]}});
    let w1 = id_of(&lab.td.agent("terra", "w1", pol.clone()).await);
    let w2 = id_of(&lab.td.agent("terra", "w2", pol).await);
    lab.target(
        json!({"name": "keep", "kind": "docker", "host": "lab1", "project": "terra",
                      "image": "alpine:3"}),
    )
    .await;
    start_agent(&lab, &w1).await;
    let up = lab.td.post_empty("/api/targets/terra-keep/up").await;
    assert_eq!(up.ok()["state"], "running");
    assert!(runs(&lab)[0].contains("/work/terra/w1"));
    start_agent(&lab, &w2).await;
    let errs = on_agent_start(&state, &w2, &["terra-keep".into()]).await;
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(
        errs[0]
            .1
            .message
            .contains("attach needs a restart of the target")
    );
    assert!(errs[0].1.message.contains("it is permanent"));
    assert!(!lab.log().contains("docker rm"), "{}", lab.log());
    // The only holder of a permanent target that runs without its
    // workspace hears it too.
    stop_agent(&lab, &w1).await;
    stop_agent(&lab, &w2).await;
    let w3 = id_of(
        &lab.td
            .agent(
                "terra",
                "w3",
                json!({"policy": {"targets": ["terra-keep"]}}),
            )
            .await,
    );
    start_agent(&lab, &w3).await;
    let errs = on_agent_start(&state, &w3, &["terra-keep".into()]).await;
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(
        errs[0]
            .1
            .message
            .contains("attach needs a restart of the target")
    );
    assert!(
        errs[0].1.message.contains("terra/w3"),
        "{}",
        errs[0].1.message
    );
}

/// A bare git repo with `alpine-box/vmlab.wcl` (the allowlist passes) and
/// `bad/vmlab.wcl` (a host path), as in `./repos/labs.git`.
fn labs_repo(dir: &Path) -> PathBuf {
    let seed = dir.join("seed");
    let lab = "import <vmlab.wcl>\nlab \"alpine-box\" {\n  container \"box\" {\n    image = \"alpine:3.22\"\n    profile = \"container\"\n    mode = :idle\n  }\n}\n";
    std::fs::create_dir_all(seed.join("alpine-box")).unwrap();
    std::fs::create_dir_all(seed.join("bad")).unwrap();
    std::fs::write(seed.join("alpine-box/vmlab.wcl"), lab).unwrap();
    std::fs::write(
        seed.join("bad/vmlab.wcl"),
        lab.replace(
            "mode = :idle",
            "mode = :idle\n    volume { host = \"/home\" target = \"/h\" }",
        ),
    )
    .unwrap();
    git(&seed, &["init", "-q", "-b", "main"]);
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "labs"]);
    git(dir, &["clone", "-q", "--bare", "seed", "labs.git"]);
    dir.join("labs.git")
}

/// A `lab_repo` target: on `up`, the host agent makes a git worktree of the
/// repo at the ref, checks the lab against its allowlist, attaches the
/// holder's workspace with `@dev`, `vmlab validate`s it, and runs `vmlab up`
/// in the instance. The stop destroys the lab and removes the worktree. A
/// lab with a host path is refused.
#[tokio::test]
async fn a_lab_repo_target_runs_in_a_worktree_with_the_workspace() {
    use terrariumd::executor::{on_agent_start, on_agent_stop};
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    let state = lab.state();
    lab.td.project("terra").await;
    let repos = tempfile::tempdir().unwrap();
    let bare = labs_repo(repos.path());
    let r = lab
        .td
        .post(
            "/api/projects/terra/repos",
            json!({"name": "labs", "url": format!("file://{}", bare.display())}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let w1 = id_of(
        &lab.td
            .agent(
                "terra",
                "w1",
                json!({"policy": {"targets": ["terra-vbox"]}}),
            )
            .await,
    );
    lab.target(
        json!({"name": "vbox", "kind": "vmlab", "host": "lab1", "project": "terra",
                      "lab_repo": "labs", "lab_ref": "main", "lab_path": "alpine-box",
                      "machine": "box", "ephemeral": true}),
    )
    .await;
    start_agent(&lab, &w1).await;
    let errs = on_agent_start(&state, &w1, &["terra-vbox".into()]).await;
    assert!(errs.is_empty(), "{errs:?}");
    let inst = lab.bins.path().join("instances/terra-vbox");
    let labdir = inst.join("lab");
    assert!(
        inst.join("src/alpine-box/vmlab.wcl").is_file(),
        "the worktree"
    );
    let wcl = std::fs::read_to_string(labdir.join("vmlab.wcl")).unwrap();
    let work = std::fs::canonicalize(lab.td.state.with_file_name("work/terra/w1")).unwrap();
    assert!(
        wcl.contains(&format!(
            "@dev(workspace = \"{}\", workspace_guest = \"/work/terra/w1\")",
            work.display()
        )),
        "{wcl}"
    );
    let log = lab.log();
    let v = log
        .find(&format!("vmlab validate (in {})", labdir.display()))
        .expect("validate");
    let u = log
        .find(&format!("vmlab up box (in {})", labdir.display()))
        .expect("up");
    assert!(v < u, "{log}");
    let t = lab.td.get("/api/targets/terra-vbox").await;
    assert_eq!(t.ok()["state"], "running");
    assert_eq!(t.body["runtime"]["attached"], json!(["terra/w1"]));
    assert_eq!(t.body["runtime"]["lab"], labdir.display().to_string());
    assert_eq!(t.body["runtime"]["head"].as_str().unwrap().len(), 40);
    // Exec goes to the instance.
    let sid = exec(&lab.td, "terra-vbox", json!({"cmd": ["uname"]})).await;
    let _ = drain(&lab.td, "terra-vbox", &sid).await;
    assert!(lab.log().contains(&format!(
        "vmlab exec box -- uname (in {})",
        labdir.display()
    )));
    // Snapshots go to the instance, as the verbs do. (The fake status
    // shows the machine `box` from now on.)
    std::fs::write(lab.bins.path().join("box"), "").unwrap();
    lab.td
        .post("/api/targets/terra-vbox/snapshot", json!({"name": "s1"}))
        .await
        .ok();
    let v = lab.td.get("/api/targets/terra-vbox/snapshots").await;
    assert_eq!(names(v.ok()), ["s1"]);
    assert_eq!(v.body["snapshots"][0]["managed"], true);
    let log = lab.log();
    for want in [
        format!(
            "vmlab snapshot create --vm box s1 (in {})",
            labdir.display()
        ),
        format!("vmlab snapshot list box (in {})", labdir.display()),
    ] {
        assert!(log.contains(&want), "missing {want:?} in\n{log}");
    }
    // The stop: destroy, then the worktree and the instance go.
    stop_agent(&lab, &w1).await;
    assert!(
        on_agent_stop(&state, &w1, &["terra-vbox".into()])
            .await
            .is_empty()
    );
    assert!(
        lab.log()
            .contains(&format!("vmlab destroy (in {})", labdir.display()))
    );
    assert!(!inst.exists(), "the instance dir goes");
    let t = lab.td.get("/api/targets/terra-vbox").await;
    assert_eq!(t.ok()["state"], "absent");
    assert_eq!(t.body["runtime"], json!({}));

    // A lab with a host path is refused, and nothing runs.
    lab.target(
        json!({"name": "vbad", "kind": "vmlab", "host": "lab1", "project": "terra",
                      "lab_repo": "labs", "lab_path": "bad", "machine": "box"}),
    )
    .await;
    let r = lab.td.post_empty("/api/targets/terra-vbad/up").await;
    r.err(422, "invalid");
    assert_eq!(r.body["error"]["details"]["code"], "refused", "{}", r.body);
    let msg = r.body["error"]["message"].as_str().unwrap();
    assert!(msg.contains("binds a host directory"), "{msg}");
    assert!(!lab.log().contains("vmlab up box (in") || lab.log().matches("vmlab up").count() == 1);
}

/// A `lab_from_workspace` target: each holder runs its own lab instance
/// from the `vmlab.wcl` in its own workspace. An exclusive lease does not
/// apply. The operator names the holder; an agent gets its own.
#[tokio::test]
async fn lab_from_workspace_gives_each_holder_its_own_instance() {
    use terrariumd::executor::{on_agent_start, on_agent_stop};
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    let state = lab.state();
    lab.td.project("terra").await;
    let pol = json!({"policy": {"targets": ["terra-vws"]}});
    let w1 = id_of(&lab.td.agent("terra", "w1", pol.clone()).await);
    let w2 = id_of(&lab.td.agent("terra", "w2", pol).await);
    let w3 = id_of(&lab.td.agent("terra", "w3", json!({})).await);
    let work = lab.td.state.with_file_name("work");
    let wcl = "import <vmlab.wcl>\nlab \"dev\" {\n  container \"box\" {\n    image = \"alpine:3.22\"\n    profile = \"container\"\n    mode = :idle\n  }\n}\n";
    for n in ["w1", "w2"] {
        std::fs::create_dir_all(work.join("terra").join(n).join("app")).unwrap();
        std::fs::write(work.join("terra").join(n).join("app/vmlab.wcl"), wcl).unwrap();
    }
    lab.target(
        json!({"name": "vws", "kind": "vmlab", "host": "lab1", "project": "terra",
                      "lab_from_workspace": true, "lab_path": "app", "machine": "box",
                      "ephemeral": true}),
    )
    .await;
    for w in [&w1, &w2] {
        start_agent(&lab, w).await;
        let errs = on_agent_start(&state, w, &["terra-vws".into()]).await;
        assert!(errs.is_empty(), "{errs:?}");
    }
    let t = lab.td.get("/api/targets/terra-vws").await;
    let inst = t.ok()["runtime"]["instances"].as_object().unwrap().clone();
    assert_eq!(inst.len(), 2, "{inst:?}");
    let lab1 = lab
        .bins
        .path()
        .join(format!("instances/terra-vws.{w1}/lab"));
    let lab2 = lab
        .bins
        .path()
        .join(format!("instances/terra-vws.{w2}/lab"));
    assert_eq!(inst[&w1]["lab"], lab1.display().to_string());
    assert_eq!(inst[&w1]["agent_name"], "terra/w1");
    assert_eq!(inst[&w1]["attached"], json!(["terra/w1"]));
    let gen1 = std::fs::read_to_string(lab1.join("vmlab.wcl")).unwrap();
    assert!(
        gen1.contains("workspace_guest = \"/work/terra/w1\""),
        "{gen1}"
    );
    // Nothing is written into the workspace.
    assert_eq!(
        std::fs::read_dir(work.join("terra/w1/app"))
            .unwrap()
            .count(),
        1
    );
    // An agent execs in its own instance; the operator names the holder.
    let r = lab
        .td
        .post_as(&w2, "/api/targets/terra-vws/exec", json!({"cmd": ["id"]}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let _ = drain_as(
        &lab.td,
        Some(&w2),
        "terra-vws",
        r.body["session_id"].as_str().unwrap(),
    )
    .await;
    assert!(
        lab.log()
            .contains(&format!("vmlab exec box -- id (in {})", lab2.display()))
    );
    lab.td
        .post("/api/targets/terra-vws/exec", json!({"cmd": ["id"]}))
        .await
        .err(422, "invalid");
    let r = lab
        .td
        .post(
            "/api/targets/terra-vws/exec",
            json!({"cmd": ["hostname"], "agent": "terra/w1"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let _ = drain(&lab.td, "terra-vws", r.body["session_id"].as_str().unwrap()).await;
    assert!(lab.log().contains(&format!(
        "vmlab exec box -- hostname (in {})",
        lab1.display()
    )));
    // Snapshots: an agent acts on its own instance, also when it names
    // another holder; the operator names the holder. (The fake status
    // shows the machine `box` from now on.)
    std::fs::write(lab.bins.path().join("box"), "").unwrap();
    lab.td
        .post_as(
            &w2,
            "/api/targets/terra-vws/snapshot",
            json!({"name": "s2"}),
        )
        .await
        .ok();
    assert!(lab.log().contains(&format!(
        "vmlab snapshot create --vm box s2 (in {})",
        lab2.display()
    )));
    let v = lab
        .td
        .get_as(&w2, "/api/targets/terra-vws/snapshots?agent=terra/w1")
        .await;
    let v = v.ok();
    assert_eq!(v["agent"], "terra/w2");
    assert_eq!(v["snapshots"][0]["name"], "s2");
    assert!(
        lab.log()
            .contains(&format!("vmlab snapshot list box (in {})", lab2.display()))
    );
    lab.td
        .get("/api/targets/terra-vws/snapshots")
        .await
        .err(422, "invalid");
    let v = lab
        .td
        .get("/api/targets/terra-vws/snapshots?agent=terra/w1")
        .await;
    assert_eq!(v.ok()["agent"], "terra/w1");
    assert!(
        lab.log()
            .contains(&format!("vmlab snapshot list box (in {})", lab1.display()))
    );
    let r = delete_as(
        &lab.td,
        &w2,
        "/api/targets/terra-vws/snapshots/s2?agent=terra/w1",
    )
    .await;
    assert_eq!(r.ok()["agent"], "terra/w2");
    assert!(lab.log().contains(&format!(
        "vmlab snapshot delete box s2 (in {})",
        lab2.display()
    )));
    // w3 may not use the target.
    lab.td
        .get_as(&w3, "/api/targets/terra-vws/snapshots")
        .await
        .err(403, "forbidden");
    // No exclusive lease on a per-holder target.
    lab.td
        .post(
            "/api/targets/terra-vws/leases",
            json!({"agent": "terra/w3", "exclusive": true}),
        )
        .await
        .err(422, "invalid");
    let _ = w3;
    // The stop of w1 removes its instance only.
    stop_agent(&lab, &w1).await;
    assert!(
        on_agent_stop(&state, &w1, &["terra-vws".into()])
            .await
            .is_empty()
    );
    assert!(
        lab.log()
            .contains(&format!("vmlab destroy (in {})", lab1.display()))
    );
    assert!(!lab1.exists());
    assert!(lab2.join("vmlab.wcl").is_file());
    let t = lab.td.get("/api/targets/terra-vws").await;
    let inst = t.ok()["runtime"]["instances"].as_object().unwrap().clone();
    assert_eq!(inst.keys().collect::<Vec<_>>(), [&w2]);
}

/// `TERRARIUM_MAX_TARGETS_PER_PROJECT` (8) and the host's
/// `max_running_targets`, and the project prefix of an API target.
#[tokio::test]
async fn target_caps_and_the_project_prefix() {
    let lab = Lab::start().await;
    let token = lab.register("small").await;
    let mut c = lab.client("small", &token);
    let _ = &mut c;
    let task = {
        let mut o = ClientOptions::new(
            format!("ws://{}/hosts/connect", lab.addr),
            token.clone(),
            "small",
        );
        o.capabilities = HostCapabilities {
            docker: ToolCapability {
                present: true,
                version: Some("29.8.0".into()),
            },
            vmlab: ToolCapability::default(),
            shell: Some("/bin/sh".into()),
            ..Default::default()
        };
        o.docker_bin = lab.bins.path().join("docker").display().to_string();
        o.max_running_targets = 1;
        tokio::spawn(Client::new(o).run())
    };
    lab.wait_online("small", true).await;
    let h = lab.td.get("/api/hosts/small").await;
    assert_eq!(h.ok()["max_running_targets"], 1, "{}", h.body);
    lab.td.project("terra").await;
    for i in 0..8 {
        let r = lab
            .td
            .post(
                "/api/targets",
                json!({"name": format!("t{i}"), "kind": "docker", "host": "small",
                                        "project": "terra", "image": "alpine:3"}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
        assert_eq!(r.body["name"], format!("terra-t{i}"));
    }
    let r = lab
        .td
        .post(
            "/api/targets",
            json!({"name": "t8", "kind": "docker", "host": "small",
                                    "project": "terra", "image": "alpine:3"}),
        )
        .await;
    r.err(409, "conflict");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("TERRARIUM_MAX_TARGETS_PER_PROJECT")
    );
    // A name with the prefix keeps it; a global target has none.
    lab.td.delete("/api/targets/terra-t7").await;
    let r = lab
        .td
        .post(
            "/api/targets",
            json!({"name": "terra-x", "kind": "host", "host": "small", "project": "terra"}),
        )
        .await;
    assert_eq!(r.ok()["name"], "terra-x");
    let r = lab
        .td
        .post(
            "/api/targets",
            json!({"name": "g", "kind": "host", "host": "small"}),
        )
        .await;
    assert_eq!(r.ok()["name"], "g");
    assert_eq!(r.body["scope"], "global");
    // One running target on the host: the second `up` is 429.
    lab.td.post_empty("/api/targets/terra-t0/up").await.ok();
    let r = lab.td.post_empty("/api/targets/terra-t1/up").await;
    r.err(429, "rate_limited");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("max_running_targets is 1")
    );
    lab.td
        .post("/api/targets/terra-t0/down", json!({"destroy": true}))
        .await
        .ok();
    lab.td.post_empty("/api/targets/terra-t1/up").await.ok();
    task.abort();
}

/// Schedules (docs/api.md §19): an `exec` action runs through the
/// executor as the schedule's `run_as`, with the same policy and audit as
/// any exec. The run keeps the tail of the output and the exit code.
#[tokio::test]
async fn scheduled_exec_runs_through_the_executor() {
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    lab.target(json!({"name": "box", "kind": "host", "host": "lab1"}))
        .await;
    lab.target(json!({"name": "dbox", "kind": "docker", "host": "lab1",
                      "image": "alpine:3"}))
        .await;
    lab.td.project("terra").await;
    let lead = id_of(
        &lab.td
            .agent("terra", "lead", json!({"roles": ["project_admin"]}))
            .await,
    );
    let exec = |name: &str, target: &str, cmd: Value| {
        json!({"name": name, "project": "terra", "spec": {"kind": "interval", "every": "1h"},
               "action": {"type": "exec", "target": target, "cmd": cmd, "timeout_secs": 30}})
    };
    let r = lab
        .td
        .post(
            "/api/schedules",
            exec(
                "check",
                "box",
                json!(["sh", "-c", "echo out; echo err >&2"]),
            ),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let run = lab.td.post_empty("/api/schedules/terra%2Fcheck/run").await;
    let run = run.ok();
    assert_eq!(run["status"], "ok", "{run}");
    assert_eq!(run["output"]["exit_code"], 0);
    assert_eq!(run["output"]["stdout"], "out\n");
    assert_eq!(run["output"]["stderr"], "err\n");
    let row = last_audit(&lab.td, "target.exec").await;
    assert_eq!(row["actor"], "operator");
    assert_eq!(row["target"], "box");
    assert!(
        row["detail"]["command"]
            .as_str()
            .unwrap()
            .starts_with("sh -c echo out")
    );
    assert_eq!(
        lab.state().executor.session_count(),
        0,
        "the run closes its session"
    );

    // A non-zero exit fails the run, with the code.
    lab.td
        .post(
            "/api/schedules",
            exec("bad", "box", json!(["sh", "-c", "exit 3"])),
        )
        .await
        .ok();
    let run = lab.td.post_empty("/api/schedules/terra%2Fbad/run").await;
    assert_eq!(run.ok()["status"], "failed");
    assert_eq!(run.body["error"], "exit code 3");
    assert_eq!(run.body["output"]["exit_code"], 3);

    // A docker target: the recording fake docker gets the exec.
    lab.td.post_empty("/api/targets/dbox/up").await.ok();
    lab.td
        .post(
            "/api/schedules",
            exec("in-docker", "dbox", json!(["make", "test"])),
        )
        .await
        .ok();
    let runs = {
        let mut out = Vec::new();
        let at = chrono::Utc::now() + chrono::Duration::hours(2);
        for h in terrariumd::scheduler::tick(&lab.state(), at).await {
            out.push(h.await.unwrap().unwrap());
        }
        out
    };
    assert_eq!(runs.len(), 3, "{runs:?}");
    let docker = runs.iter().find(|r| r.output["target"] == "dbox").unwrap();
    assert_eq!(docker.status, "ok", "{docker:?}");
    assert!(
        // The command runs behind the PID-file wrapper (TER-99).
        lab.log().contains("docker exec")
            && lab.log().contains("terrarium-dbox sh -c")
            && lab.log().contains("terrarium-run /tmp/terrarium-run-")
            && lab.log().contains(".pid make test"),
        "{}",
        lab.log()
    );

    // The lead may schedule an exec on a global target, but at run time
    // the executor checks the lead's own policy and leases: the run
    // fails with the policy error.
    let r = lab
        .td
        .post_as(
            &lead,
            "/api/schedules",
            exec("mine", "box", json!(["true"])),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let run = lab.td.post_empty("/api/schedules/terra%2Fmine/run").await;
    let run = run.ok();
    assert_eq!(run["status"], "failed", "{run}");
    assert!(
        run["error"].as_str().unwrap().contains("policy.targets"),
        "{run}"
    );
    // The lead cannot borrow the operator's rights: a run-now of the
    // operator's exec on a target that the lead may not use is refused.
    lab.td
        .post_empty_as(&lead, "/api/schedules/terra%2Fcheck/run")
        .await
        .err(403, "forbidden");
    // A lease lets it in.
    lab.td
        .post(
            "/api/targets/box/leases",
            json!({"agent": lead, "until": "30m"}),
        )
        .await
        .ok();
    let run = lab.td.post_empty("/api/schedules/terra%2Fmine/run").await;
    assert_eq!(run.ok()["status"], "ok", "{}", run.body);
    let row = last_audit(&lab.td, "target.exec").await;
    assert_eq!(row["actor"], format!("agent:{lead}"));
    let run = lab
        .td
        .post_empty_as(&lead, "/api/schedules/terra%2Fcheck/run")
        .await;
    assert_eq!(run.ok()["status"], "ok", "{}", run.body);
    assert_eq!(run.body["actor"], format!("agent:{lead}"));
}

// ------------------------------------------------------------ resource limits

fn full_limits_cap() -> LimitsCapability {
    LimitsCapability {
        systemd: false,
        controllers: Vec::new(),
        reason: Some("systemd-run is off in this test".into()),
        docker: true,
    }
}

#[tokio::test]
async fn target_limits_validate_and_merge() {
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    lab.target(json!({"name": "box", "kind": "host", "host": "lab1",
                      "limits": {"cpu": 2, "memory": "4G"}}))
        .await;
    let t = lab.td.get("/api/targets/box").await;
    assert_eq!(t.ok()["limits"], json!({"cpu": 2, "memory": "4G"}));
    assert_eq!(t.body["limits_info"]["summary"], "2 CPU · 4G");
    assert_eq!(t.body["limits_info"]["per"], "holder");
    let bad = [
        (
            json!({"kind": "host", "host": "lab1", "limits": {"memory": "4X"}}),
            422,
            "limits.memory",
        ),
        (
            json!({"kind": "host", "host": "lab1", "limits": {"cpu": 0}}),
            422,
            "limits.cpu",
        ),
        (
            json!({"kind": "host", "host": "lab1", "limits": {"io_weight": 0}}),
            422,
            "io_weight",
        ),
        (
            json!({"kind": "host", "host": "lab1", "limits": {"swap": "1G"}}),
            422,
            "needs `limits.memory`",
        ),
        (
            json!({"kind": "ssh", "address": "u@h", "limits": {"cpu": 1}}),
            422,
            "ssh target",
        ),
        (
            json!({"kind": "vmlab", "host": "lab1", "lab": "/labs/x", "limits": {"cpu": 1}}),
            422,
            "vmlab",
        ),
        (
            json!({"kind": "docker", "host": "lab1", "container": "c1", "limits": {"cpu": 1}}),
            422,
            "needs `image`",
        ),
        (
            json!({"kind": "desktop", "host": "lab1", "limits": {"cpu": 1}}),
            422,
            "only a display",
        ),
        (
            json!({"kind": "host", "host": "lab1", "limits": {"memroy": "1G"}}),
            400,
            "unknown field",
        ),
    ];
    for (mut def, status, text) in bad {
        def["name"] = "bad".into();
        let r = lab.td.post("/api/targets", def.clone()).await;
        assert_eq!(r.status, status, "{def}: {}", r.body);
        let msg = r.body["error"]["message"].as_str().unwrap();
        assert!(msg.contains(text), "{def}: {msg}");
    }
    // A host target is `running` while its host agent is online; its
    // limits still change, and a patch merges into them.
    lab.td.post_empty("/api/targets/box/status").await.ok();
    assert_eq!(
        lab.td.get("/api/targets/box").await.ok()["state"],
        "running"
    );
    let r = lab
        .td
        .patch(
            "/api/targets/box",
            json!({"limits": {"pids": 64, "cpu": null}}),
        )
        .await;
    assert_eq!(r.ok()["limits"], json!({"memory": "4G", "pids": 64}));
    assert_eq!(r.body["limits_info"]["summary"], "4G · 64 pids");
    let r = lab
        .td
        .patch("/api/targets/box", json!({"limits": {"memory": "1M"}}))
        .await;
    assert_eq!(r.status, 422, "{}", r.body);
    // Removing the last limit leaves no `limits`.
    let r = lab
        .td
        .patch(
            "/api/targets/box",
            json!({"limits": {"memory": null, "pids": null}}),
        )
        .await;
    assert!(r.ok().get("limits").is_none(), "{}", r.body);
    assert!(r.body.get("limits_info").is_none());
    let r = lab
        .td
        .patch("/api/targets/box", json!({"limits": {"cpu": 0.5}}))
        .await;
    assert_eq!(r.ok()["limits"], json!({"cpu": 0.5}));
    let r = lab
        .td
        .patch("/api/targets/box", json!({"limits": null}))
        .await;
    assert!(r.ok().get("limits").is_none(), "{}", r.body);
}

#[tokio::test]
async fn an_old_host_agent_gets_no_limited_session() {
    let lab = Lab::start().await;
    // No `limits` capability: a host agent from before the feature.
    let _h = lab.host("lab1").await;
    lab.target(json!({"name": "box", "kind": "host", "host": "lab1",
                      "limits": {"memory": "1G"}}))
        .await;
    lab.target(json!({"name": "dbox", "kind": "docker", "host": "lab1",
                      "image": "alpine:3", "limits": {"cpu": 1}}))
        .await;
    let t = lab.td.get("/api/targets/box").await;
    let problem = t.ok()["limits_info"]["problem"].as_str().unwrap();
    assert!(
        problem.starts_with("this host agent cannot apply resource limits: it is older"),
        "{problem}"
    );
    let r = lab
        .td
        .post("/api/targets/box/exec", json!({"cmd": ["true"]}))
        .await;
    assert_eq!(r.status, 409, "{}", r.body);
    let msg = r.body["error"]["message"].as_str().unwrap();
    assert!(msg.contains("cannot apply resource limits"), "{msg}");
    let r = lab.td.post_empty("/api/targets/dbox/up").await;
    assert_eq!(r.status, 409, "{}", r.body);
    assert!(!lab.log().contains("docker run"), "{}", lab.log());
    // A target with no limits still runs.
    lab.target(json!({"name": "free", "kind": "host", "host": "lab1"}))
        .await;
    let sid = exec(&lab.td, "free", json!({"cmd": ["true"]})).await;
    assert_eq!(drain(&lab.td, "free", &sid).await.end["code"], 0);
}

#[tokio::test]
async fn docker_limits_go_on_the_container() {
    let lab = Lab::start().await;
    let _h = lab.host_with("lab1", Some(full_limits_cap())).await;
    lab.target(
        json!({"name": "dbox", "kind": "docker", "host": "lab1", "image": "alpine:3",
                      "limits": {"cpu": 1.5, "memory": "512M", "pids": 128, "io_weight": 100}}),
    )
    .await;
    let t = lab.td.get("/api/targets/dbox").await;
    assert_eq!(t.ok()["limits_info"]["per"], "container");
    assert!(t.body["limits_info"].get("problem").is_none(), "{}", t.body);
    lab.td.post_empty("/api/targets/dbox/up").await.ok();
    let run = lab
        .log()
        .lines()
        .find(|l| l.starts_with("docker run"))
        .unwrap()
        .to_owned();
    assert!(
        run.contains(
            "--cpus=1.5 --memory=536870912 --memory-swap=536870912 --pids-limit=128 \
             --blkio-weight=20"
        ),
        "{run}"
    );
    // Changed create-time settings require explicit consent to recreate.
    lab.td.post_empty("/api/targets/dbox/down").await.ok();
    let r = lab
        .td
        .patch("/api/targets/dbox", json!({"limits": {"cpu": 2}}))
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["runtime"]["recreate_required"], true);
    let old = lab.td.post_empty("/api/targets/dbox/up").await.ok().clone();
    assert!(
        old["recreate_note"]
            .as_str()
            .unwrap()
            .contains("old image and limits")
    );
    lab.td.post_empty("/api/targets/dbox/down").await.ok();
    lab.td.post_empty("/api/targets/dbox/recreate").await.ok();
    assert!(lab.log().contains("--cpus=2 "), "{}", lab.log());
    // The same limits are no change.
    lab.td.post_empty("/api/targets/dbox/down").await.ok();
    let r = lab
        .td
        .patch(
            "/api/targets/dbox",
            json!({"limits": {"cpu": 2}, "keep": true}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    // A host target on this host agent: systemd is off, so it is refused
    // with the host agent's reason.
    lab.target(json!({"name": "box", "kind": "host", "host": "lab1",
                      "limits": {"cpu": 1}}))
        .await;
    let r = lab
        .td
        .post("/api/targets/box/exec", json!({"cmd": ["true"]}))
        .await;
    assert_eq!(r.status, 409, "{}", r.body);
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("systemd-run is off in this test")
    );
}

#[tokio::test]
async fn docker_image_edit_needs_explicit_recreate_without_volume_deletion() {
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    lab.target(json!({"name": "dbox", "kind": "docker", "host": "lab1",
                      "image": "alpine:3"}))
        .await;
    lab.td.post_empty("/api/targets/dbox/up").await.ok();
    lab.td
        .post_empty("/api/targets/dbox/recreate")
        .await
        .err(409, "conflict");
    assert!(!lab.log().contains("docker rm"));
    lab.td.post_empty("/api/targets/dbox/down").await.ok();
    let unchanged = lab
        .td
        .patch("/api/targets/dbox", json!({"image": "alpine:3"}))
        .await
        .ok()
        .clone();
    assert!(unchanged["recreate_note"].is_null());
    let edited = lab
        .td
        .patch("/api/targets/dbox", json!({"image": "alpine:latest"}))
        .await
        .ok()
        .clone();
    assert!(
        edited["recreate_note"]
            .as_str()
            .unwrap()
            .contains("writable layer")
    );
    // An unrelated patch must not clear the persisted requirement.
    let edited = lab
        .td
        .patch("/api/targets/dbox", json!({"keep": true}))
        .await
        .ok()
        .clone();
    assert_eq!(edited["runtime"]["recreate_required"], true);
    let old = lab.td.post_empty("/api/targets/dbox/up").await.ok().clone();
    assert!(
        old["recreate_note"]
            .as_str()
            .unwrap()
            .contains("old image and limits")
    );
    lab.td.post_empty("/api/targets/dbox/down").await.ok();
    let recreated = lab
        .td
        .post_empty("/api/targets/dbox/recreate")
        .await
        .ok()
        .clone();
    assert_eq!(recreated["verb"], "recreate");
    assert_eq!(recreated["state"], "running");
    let log = lab.log();
    assert!(log.contains("docker rm terrarium-dbox"), "{log}");
    assert!(!log.contains("docker rm -"), "{log}");
    assert!(
        log.lines()
            .rfind(|l| l.starts_with("docker run"))
            .unwrap()
            .contains("alpine:latest"),
        "{log}"
    );
    assert_eq!(log.matches("docker run ").count(), 2, "{log}");
    assert!(lab.td.get("/api/targets/dbox").await.ok()["recreate_note"].is_null());
    lab.td.post_empty("/api/targets/dbox/down").await.ok();
    lab.td.post_empty("/api/targets/dbox/up").await.ok();
    assert_eq!(lab.log().matches("docker run ").count(), 2);
    assert!(lab.log().contains("docker start terrarium-dbox"));
}

#[tokio::test]
async fn docker_recreate_requires_admin_and_refuses_live_sessions() {
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    lab.td.project("terra").await;
    let worker = lab
        .td
        .agent("terra", "worker", json!({"policy": {"targets": ["dbox"]}}))
        .await;
    let worker = worker["id"].as_str().unwrap();
    lab.target(json!({"name": "dbox", "kind": "docker", "host": "lab1", "image": "alpine:3"}))
        .await;
    lab.td
        .req(
            hyper::Method::POST,
            "/api/targets/dbox/recreate",
            None,
            Some(worker),
        )
        .await
        .err(403, "forbidden");
    lab.td.post_empty("/api/targets/dbox/up").await.ok();
    let sid = exec(&lab.td, "dbox", json!({"cmd": ["sleeper"]})).await;
    lab.td.post_empty("/api/targets/dbox/down").await.ok();
    lab.td
        .post_empty("/api/targets/dbox/recreate")
        .await
        .err(409, "conflict");
    assert!(!lab.log().contains("docker rm"));
    std::fs::write(lab.bins.path().join("sleeper.end"), "").unwrap();
    drain(&lab.td, "dbox", &sid).await;
    lab.td.post_empty("/api/targets/dbox/recreate").await.ok();
    lab.target(
        json!({"name": "external", "kind": "docker", "host": "lab1", "container": "existing"}),
    )
    .await;
    lab.td
        .post_empty("/api/targets/external/recreate")
        .await
        .err(422, "invalid");
}

#[tokio::test]
async fn docker_recreate_refuses_mounts_and_unknown_mount_metadata() {
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    lab.target(json!({"name": "dbox", "kind": "docker", "host": "lab1", "image": "alpine:3"}))
        .await;
    lab.td.post_empty("/api/targets/dbox/up").await.ok();
    lab.td.post_empty("/api/targets/dbox/down").await.ok();
    for mounts in [
        json!([{"Type": "volume", "Name": "data", "Destination": "/data"}]),
        json!([{"Type": "bind", "Source": "/tmp/data", "Destination": "/data"}]),
        Value::Null,
    ] {
        std::fs::write(lab.bins.path().join("docker.mounts"), mounts.to_string()).unwrap();
        lab.td
            .post_empty("/api/targets/dbox/recreate")
            .await
            .err(409, "conflict");
        assert!(!lab.log().contains("docker rm"));
        assert_eq!(lab.log().matches("docker run ").count(), 1);
    }
}

#[tokio::test]
async fn docker_pending_edits_can_be_undone_after_recreate_refusal() {
    let lab = Lab::start().await;
    let _h = lab.host_with("lab1", Some(full_limits_cap())).await;
    lab.target(json!({"name": "dbox", "kind": "docker", "host": "lab1", "image": "alpine:3", "limits": {"cpu": 1}})).await;
    lab.td.post_empty("/api/targets/dbox/up").await.ok();
    lab.td.post_empty("/api/targets/dbox/down").await.ok();
    std::fs::write(
        lab.bins.path().join("docker.mounts"),
        r#"[{"Type":"volume","Name":"data","Destination":"/data"}]"#,
    )
    .unwrap();
    lab.td
        .patch(
            "/api/targets/dbox",
            json!({"image": "alpine:latest", "limits": {"cpu": 2}}),
        )
        .await
        .ok();
    lab.td
        .post_empty("/api/targets/dbox/recreate")
        .await
        .err(409, "conflict");
    let old = lab.td.post_empty("/api/targets/dbox/up").await.ok().clone();
    assert!(
        old["recreate_note"]
            .as_str()
            .unwrap()
            .contains("old image and limits")
    );
    lab.td.post_empty("/api/targets/dbox/down").await.ok();
    let partial = lab
        .td
        .patch("/api/targets/dbox", json!({"image": "alpine:3"}))
        .await
        .ok()
        .clone();
    assert_eq!(partial["runtime"]["recreate_required"], true);
    let undone = lab
        .td
        .patch("/api/targets/dbox", json!({"limits": {"cpu": 1}}))
        .await
        .ok()
        .clone();
    assert!(undone["recreate_note"].is_null());
    let old = lab.td.post_empty("/api/targets/dbox/up").await.ok().clone();
    assert!(old["recreate_note"].is_null());
    assert!(!lab.log().contains("docker rm"));
    assert_eq!(lab.log().matches("docker run ").count(), 1);
}

#[tokio::test]
async fn docker_recreate_preserves_verified_workspace_binds() {
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    lab.target(json!({"name": "dbox", "kind": "docker", "host": "lab1", "image": "alpine:3"}))
        .await;
    let state = lab.state();
    let workspace = state.work_dir.join("terra/worker");
    std::fs::create_dir_all(&workspace).unwrap();
    let workspace = std::fs::canonicalize(workspace).unwrap();
    state
        .db
        .write("test", |t| {
            let mut rt = terrariumd::store::targets::get(t, "dbox")?.runtime;
            rt.attached = vec!["terra/worker".into()];
            terrariumd::store::targets::set_runtime(t, "dbox", &rt)
        })
        .await
        .unwrap();
    // Fake inspect describes the existing stopped container and its exact bind.
    std::fs::write(
        lab.bins.path().join("docker.state.terrarium-dbox"),
        "exited",
    )
    .unwrap();
    let mounts = json!([{"Type":"bind", "Source":workspace, "Destination":"/work/terra/worker", "RW":true, "Propagation":"rprivate"}]);
    std::fs::write(lab.bins.path().join("docker.mounts"), mounts.to_string()).unwrap();
    lab.td.post_empty("/api/targets/dbox/recreate").await.ok();
    let run = runs(&lab);
    assert_eq!(run.len(), 1);
    assert!(
        run[0].contains(&format!("-v {}:/work/terra/worker", workspace.display())),
        "{run:?}"
    );
    assert_eq!(
        lab.td.get("/api/targets/dbox").await.ok()["runtime"]["attached"],
        json!(["terra/worker"])
    );
}

#[tokio::test]
async fn docker_absent_edits_need_only_up_and_attach_holder_workspaces() {
    let lab = Lab::start().await;
    let _h = lab.host("lab1").await;
    lab.td.project("terra").await;
    let worker = id_of(
        &lab.td
            .agent("terra", "worker", json!({"policy":{"targets":["dbox"]}}))
            .await,
    );
    lab.target(json!({"name":"dbox", "kind":"docker", "host":"lab1", "image":"alpine:3"}))
        .await;
    // Both a new target and an inspected absent target need no recreation.
    let edit = lab
        .td
        .patch("/api/targets/dbox", json!({"image":"alpine:3.20"}))
        .await
        .ok()
        .clone();
    assert!(edit["recreate_note"].is_null());
    assert_eq!(
        lab.td.post_empty("/api/targets/dbox/status").await.ok()["state"],
        "absent"
    );
    let edit = lab
        .td
        .patch("/api/targets/dbox", json!({"image":"alpine:3.21"}))
        .await
        .ok()
        .clone();
    assert!(edit["recreate_note"].is_null());
    let refused = lab.td.post_empty("/api/targets/dbox/recreate").await;
    refused.err(409, "conflict");
    assert!(
        refused.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no container yet; up creates it")
    );
    assert!(runs(&lab).is_empty());
    start_agent(&lab, &worker).await;
    lab.td.post_empty("/api/targets/dbox/up").await.ok();
    let run = runs(&lab);
    assert_eq!(run.len(), 1);
    assert!(run[0].contains(":/work/terra/worker"), "{run:?}");
    assert!(run[0].contains("alpine:3.21"), "{run:?}");
}

#[tokio::test]
async fn docker_legacy_edit_captures_inspect_not_the_stale_definition() {
    let lab = Lab::start().await;
    let _h = lab.host_with("lab1", Some(full_limits_cap())).await;
    // Legacy definition was edited, but the old container was never replaced.
    lab.target(json!({"name":"dbox", "kind":"docker", "host":"lab1", "image":"alpine:latest", "limits":{"cpu":2}})).await;
    std::fs::write(
        lab.bins.path().join("docker.state.terrarium-dbox"),
        "exited",
    )
    .unwrap();
    std::fs::write(lab.bins.path().join("docker.image"), "alpine:3").unwrap();
    std::fs::write(lab.bins.path().join("docker.host-config"), json!({"NanoCpus":1000000000_u64,"Memory":536870912,"MemorySwap":536870912,"PidsLimit":100}).to_string()).unwrap();
    lab.td.post_empty("/api/targets/dbox/status").await.ok();
    let edited = lab
        .td
        .patch("/api/targets/dbox", json!({"image":"alpine:3.21"}))
        .await
        .ok()
        .clone();
    assert_eq!(edited["runtime"]["docker_settings"]["image"], "alpine:3");
    assert_eq!(edited["runtime"]["recreate_required"], true);
    let undone = lab
        .td
        .patch(
            "/api/targets/dbox",
            json!({"image":"alpine:3", "limits":{"cpu":1,"memory":"512M","swap":0,"pids":100}}),
        )
        .await
        .ok()
        .clone();
    assert!(undone["recreate_note"].is_null());
    assert!(lab.td.post_empty("/api/targets/dbox/up").await.ok()["recreate_note"].is_null());
    assert!(!lab.log().contains("docker rm"));
}

/// Opt-in evidence on the leased throwaway Docker VM, never on shared build Docker.
#[tokio::test]
#[ignore = "requires the leased terrarium-docker-scratch VM with Docker"]
async fn docker_real_recreate_changes_image_id_and_preserves_workspace() {
    let name = "ter-202-image-check";
    struct ContainerGuard(&'static str);
    impl Drop for ContainerGuard {
        fn drop(&mut self) {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", self.0])
                .output();
        }
    }
    let lab = Lab::start().await;
    // Map the test daemon's managed name to this card's permitted container prefix.
    std::fs::write(lab.bins.path().join("docker"), format!(
        "#!/usr/bin/python3\nimport subprocess,sys\nsys.exit(subprocess.call(['docker'] + ['{name}' if a == 'terrarium-dbox' else a for a in sys.argv[1:]]))\n"
    )).unwrap();
    let _h = lab.host("lab1").await;
    lab.target(json!({"name":"dbox", "kind":"docker", "host":"lab1", "image":"alpine:3.20"}))
        .await;
    let state = lab.state();
    std::fs::create_dir_all(state.work_dir.join("terra/worker")).unwrap();
    let def = state
        .db
        .read(|c| terrariumd::store::targets::get(c, "dbox"))
        .await
        .unwrap()
        .def;
    let _guard = ContainerGuard(name);
    terrariumd::executor::lifecycle(
        &state,
        &def,
        terrarium_core::protocol::LifecycleVerb::Up,
        &terrariumd::executor::LifecycleOpts {
            recreate_workspaces: Some(vec!["terra/worker".into()]),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let inspect = || {
        let out = std::process::Command::new("docker")
            .args(["inspect", name])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()[0].clone()
    };
    let before = inspect();
    lab.td.post_empty("/api/targets/dbox/down").await.ok();
    lab.td
        .patch("/api/targets/dbox", json!({"image":"alpine:3.21"}))
        .await
        .ok();
    lab.td.post_empty("/api/targets/dbox/recreate").await.ok();
    let after = inspect();
    println!(
        "docker inspect before: {}",
        json!({"Id":before["Id"],"Image":before["Image"],"Mounts":before["Mounts"]})
    );
    println!(
        "docker inspect after: {}",
        json!({"Id":after["Id"],"Image":after["Image"],"Mounts":after["Mounts"]})
    );
    assert_ne!(before["Image"], after["Image"]);
    assert_ne!(before["Id"], after["Id"]);
    assert_eq!(after["Config"]["Image"], "alpine:3.21");
    assert_eq!(before["Mounts"], after["Mounts"]);
    assert_eq!(after["Mounts"].as_array().unwrap().len(), 1);
}

/// The whole path on this machine: the daemon sends the limits and the
/// holder, the host agent sets them on the holder's slice and runs the
/// command in a scope there. Skips when `systemd-run --user` does not
/// work.
#[tokio::test]
async fn a_host_exec_runs_in_the_slice_of_its_holder() {
    let cap = terrarium_host::limits::detect().await;
    if !cap.systemd || !cap.controllers.iter().any(|c| c == "memory") {
        eprintln!("skip: {:?}", cap.reason);
        return;
    }
    let lab = Lab::start().await;
    let _h = lab.host_with("lab1", Some(cap)).await;
    lab.td.project("terra").await;
    let name = format!("terra-lim{}", std::process::id());
    let worker = lab
        .td
        .agent("terra", "worker", json!({"policy": {"targets": [name]}}))
        .await;
    let w = worker["id"].as_str().unwrap().to_owned();
    lab.target(
        json!({"name": name, "kind": "host", "host": "lab1", "project": "terra",
                      "limits": {"memory": "256M", "pids": 64}}),
    )
    .await;
    let read = json!({"cmd": ["sh", "-c",
        "cat /proc/self/cgroup; cat /sys/fs/cgroup$(cut -d: -f3 /proc/self/cgroup)/../memory.max"]});
    let r = lab
        .td
        .post_as(&w, &format!("/api/targets/{name}/exec"), read.clone())
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let sid = r.body["session_id"].as_str().unwrap().to_owned();
    let d = drain_as(&lab.td, Some(&w), &name, &sid).await;
    let out = String::from_utf8_lossy(&d.stdout).to_string();
    let slice = terrarium_host::limits::slice_name(&name, "terra/worker");
    let parent = format!("terrarium-{}.slice", terrarium_host::limits::escape(&name));
    let stop = || {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "stop", "--", &parent])
            .envs(terrarium_host::limits::user_bus_env())
            .status();
        let op = terrarium_host::limits::slice_name(&name, "operator");
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "revert", "--", &slice, &op])
            .envs(terrarium_host::limits::user_bus_env())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    };
    let ok = out.contains(&format!("/{slice}/terrarium-exec-lab1-"))
        && out.lines().last() == Some(&(256u64 << 20).to_string());
    if !ok {
        stop();
    }
    assert!(ok, "{out}\n{}", String::from_utf8_lossy(&d.stderr));
    // The operator has its own budget on the same target.
    let sid = exec(&lab.td, &name, read).await;
    let out = String::from_utf8_lossy(&drain(&lab.td, &name, &sid).await.stdout).to_string();
    stop();
    assert!(
        out.contains(&terrarium_host::limits::slice_name(&name, "operator")),
        "{out}"
    );
}

/// TER-41: a remote host agent with no agent workspaces (its work_dir is
/// not a directory there) says so in its capabilities. `up` of a docker
/// target on it starts the container with no /work mount (as an attach to
/// such a host does), records no attached workspace, and says so in
/// `workspace_note`. A lease to another running agent attaches nothing and
/// gives no attach error.
#[tokio::test]
async fn a_docker_target_on_a_host_without_workspaces_starts_without_mounts() {
    let lab = Lab::start().await;
    let token = lab.register("remote1").await;
    let mut o = ClientOptions::new(
        format!("ws://{}/hosts/connect", lab.addr),
        &token,
        "remote1",
    );
    o.capabilities = HostCapabilities {
        docker: ToolCapability {
            present: true,
            version: Some("29.8.0".into()),
        },
        shell: Some("/bin/sh".into()),
        no_workspaces: true,
        ..Default::default()
    };
    o.backoff_min = Duration::from_millis(50);
    o.backoff_max = Duration::from_millis(200);
    o.docker_bin = lab.bins.path().join("docker").display().to_string();
    o.vmlab_bin = lab.bins.path().join("vmlab").display().to_string();
    // No copy of the daemon's work root on this host.
    o.paths = terrarium_host::HostPaths {
        work_dir: lab.bins.path().join("no-such-work-dir"),
        instances_dir: lab.bins.path().join("instances"),
        repos_dir: lab.bins.path().join("repos"),
    };
    let _h = tokio::spawn(Client::new(o).run());
    lab.wait_online("remote1", true).await;
    lab.td.project("terra").await;
    let owner = id_of(
        &lab.td
            .agent(
                "terra",
                "owner",
                json!({"policy": {"targets": ["terra-rbox"]}}),
            )
            .await,
    );
    let w = id_of(&lab.td.agent("terra", "worker", json!({})).await);
    lab.target(
        json!({"name": "terra-rbox", "kind": "docker", "host": "remote1", "project": "terra",
                      "image": "alpine:3", "ephemeral": true}),
    )
    .await;
    for a in [&owner, &w] {
        lab.td
            .post_empty(&format!("/api/agents/{a}/start"))
            .await
            .ok();
        lab.td.wait_status(a, "idle").await;
    }
    let r = lab.td.post_empty("/api/targets/terra-rbox/up").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["state"], "running", "{}", r.body);
    let note = r.body["workspace_note"].as_str().unwrap_or_default();
    assert!(
        note.contains("no agent workspaces") && note.contains("remote1"),
        "{}",
        r.body
    );
    assert!(r.body.get("attach_error").is_none(), "{}", r.body);
    let t = lab.td.get("/api/targets/terra-rbox").await.ok().clone();
    let attached = t["runtime"].get("attached").cloned().unwrap_or(json!([]));
    assert_eq!(attached, json!([]), "{t}");
    let log = lab.log();
    let run = log
        .lines()
        .rfind(|l| l.starts_with("docker run"))
        .unwrap_or_default()
        .to_owned();
    assert!(!run.is_empty(), "{log}");
    assert!(!run.contains(":/work/"), "{run}");
    // A lease to another running agent: nothing to attach, no error.
    let r = lab
        .td
        .post(
            "/api/targets/terra-rbox/leases",
            json!({"agent": "terra/worker"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert!(r.body.get("lifecycle_error").is_none(), "{}", r.body);
    assert!(
        r.body["lifecycle"].get("attach_error").is_none() || r.body["lifecycle"].is_null(),
        "{}",
        r.body
    );
}
