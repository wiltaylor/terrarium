//! Target displays (docs/api.md §17.10): a real `terrarium_host::Client`
//! with fake `vmlab` and `docker` tools dials the daemon under test, and
//! the tests drive screenshots and input through the API.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use base64::Engine;
use common::TestDaemon;
use serde_json::{Value, json};
use terrarium_core::protocol::{DisplayCapability, HostCapabilities, ToolCapability};
use terrarium_host::{Client, ClientOptions};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// A 4x3 red PNG.
const SHOT: &str = "iVBORw0KGgoAAAANSUhEUgAAAAQAAAADCAIAAAA7ljmRAAAAEUlEQVR4nGP4z8AARwgWOgcA9TEL9VuTca0AAAAASUVORK5CYII=";

struct Lab {
    td: TestDaemon,
    addr: SocketAddr,
    bins: tempfile::TempDir,
}

/// A fake tool: it logs each call; a screenshot writes [`SHOT`].
fn fake(tool: &str, dir: &Path) -> String {
    let log = dir.join("calls.log");
    let body = match tool {
        // `vmlab vm screenshot <vm> <path>` writes the file.
        "vmlab" => format!(
            r#"if [ "$1 $2" = "vm screenshot" ]; then printf '%s' '{SHOT}' | base64 -d > "$4"; echo "$4"; fi"#
        ),
        // `docker exec ... sh -c <script>` prints the PNG on stdout.
        _ => format!(
            r#"for a in "$@"; do if [ "$a" = sh ]; then printf '%s' '{SHOT}' | base64 -d; exit 0; fi; done"#
        ),
    };
    format!(
        "#!/bin/sh\necho \"{tool} $*\" >> '{log}'\n{body}\nexit 0\n",
        log = log.display()
    )
}

impl Lab {
    async fn start() -> Self {
        let td = TestDaemon::start().await;
        let state = td.daemon.as_ref().unwrap().state.clone();
        let (_task, addr) = terrariumd::hosts::serve(state, "127.0.0.1:0")
            .await
            .unwrap();
        let bins = tempfile::tempdir().unwrap();
        for tool in ["docker", "vmlab"] {
            let p = bins.path().join(tool);
            std::fs::write(&p, fake(tool, bins.path())).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self { td, addr, bins }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.bins.path().join("calls.log")).unwrap_or_default()
    }

    /// Register and run a host agent; `desktop` opts its display in.
    async fn host(&self, name: &str, desktop: bool) {
        let r = self.td.post("/api/hosts", json!({"name": name})).await;
        assert_eq!(r.status, 201, "{}", r.body);
        let token = r.body["token"].as_str().unwrap().to_owned();
        let mut o = ClientOptions::new(format!("ws://{}/hosts/connect", self.addr), token, name);
        o.capabilities = HostCapabilities {
            windows_run_as: false,
            gpus: vec![],
            gpu_reservations: false,
            docker: ToolCapability {
                present: true,
                version: Some("29.8.0".into()),
            },
            vmlab: ToolCapability {
                present: true,
                version: Some("0.34.1".into()),
            },
            shell: Some("/bin/sh".into()),
            display: DisplayCapability {
                enabled: desktop,
                ..DisplayCapability::default()
            },
            metrics: false,
            limits: None,
            no_workspaces: false,
            capacity: None,
            reservations: false,
            native_gpu: None,
        };
        o.display.enabled = desktop;
        o.docker_bin = self.bins.path().join("docker").display().to_string();
        o.vmlab_bin = self.bins.path().join("vmlab").display().to_string();
        o.display.rdp_known_hosts = Some(self.bins.path().join("rdp-known-hosts"));
        o.backoff_min = Duration::from_millis(50);
        o.backoff_max = Duration::from_millis(200);
        tokio::spawn(Client::new(o).run());
        for _ in 0..300 {
            if self.td.get(&format!("/api/hosts/{name}")).await.body["online"] == true {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("host {name} never came online");
    }

    async fn target(&self, def: Value) {
        let r = self.td.post("/api/targets", def).await;
        assert_eq!(r.status, 201, "{}", r.body);
    }
}

fn id_of(a: &Value) -> String {
    a["id"].as_str().unwrap().to_owned()
}

/// The common set-up: project `terra`, a lead, two workers, host `lab1`,
/// and a vmlab target with a display.
async fn setup() -> (Lab, String, String, String) {
    let lab = Lab::start().await;
    lab.host("lab1", false).await;
    lab.td.project("terra").await;
    let lead = id_of(
        &lab.td
            .agent("terra", "lead", json!({"roles": ["project_admin"]}))
            .await,
    );
    let w = id_of(&lab.td.agent("terra", "worker", json!({})).await);
    let w2 = id_of(&lab.td.agent("terra", "w2", json!({})).await);
    let labdir = lab.bins.path().join("win");
    std::fs::create_dir_all(&labdir).unwrap();
    lab.target(
        json!({"name": "terra-win", "kind": "vmlab", "host": "lab1", "project": "terra",
                      "lab": labdir, "machine": "win11"}),
    )
    .await;
    (lab, lead, w, w2)
}

#[tokio::test]
async fn screenshots_come_as_png_or_json_and_the_view_has_a_display() {
    let (lab, ..) = setup().await;
    let view = lab.td.get("/api/targets/terra-win").await;
    assert_eq!(view.ok()["has_display"], true);

    let r = lab
        .td
        .post("/api/targets/terra-win/display/screenshot", json!({}))
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.headers["content-type"], "image/png");
    assert_eq!(r.headers["x-display-width"], "4");
    assert_eq!(r.headers["x-screen-height"], "3");
    assert_eq!(r.headers["cache-control"], "no-store");

    let j = lab
        .td
        .post(
            "/api/targets/terra-win/display/screenshot?format=json",
            json!({"scale": 0.5}),
        )
        .await;
    let j = j.ok();
    assert_eq!(j["target"], "terra-win");
    assert_eq!(
        (j["width"].as_u64(), j["height"].as_u64()),
        (Some(2), Some(2))
    );
    assert_eq!(j["screen"]["width"], 4);
    let png = B64.decode(j["png_base64"].as_str().unwrap()).unwrap();
    assert_eq!(&png[1..4], b"PNG");
    assert_eq!(j["bytes"], png.len());

    let info = lab.td.get("/api/targets/terra-win/display/info").await;
    let info = info.ok();
    assert_eq!(info["backend"], "vmlab");
    assert_eq!(info["kind"], "vmlab");
    assert_eq!(info["input"]["allowed"], true);
    assert_eq!(info["ocr"], "vmlab");

    // The vmlab command lines ran in the lab dir, with the machine.
    assert!(
        lab.log().contains("vmlab vm screenshot win11 "),
        "{}",
        lab.log()
    );

    // An unknown operation, an unknown field, and a bad value.
    lab.td
        .post("/api/targets/terra-win/display/fly", json!({}))
        .await
        .err(404, "not_found");
    lab.td
        .post(
            "/api/targets/terra-win/display/click",
            json!({"x": 1, "y": 2, "buton": "right"}),
        )
        .await
        .err(422, "invalid");
    lab.td
        .post(
            "/api/targets/terra-win/display/move",
            json!({"x": -1, "y": 2}),
        )
        .await
        .err(422, "invalid");
    // vmlab has no scroll: the host agent says so.
    let r = lab
        .td
        .post("/api/targets/terra-win/display/scroll", json!({"dy": 3}))
        .await;
    r.err(422, "invalid");
    assert_eq!(r.body["error"]["details"]["code"], "unsupported");
}

#[tokio::test]
async fn input_needs_an_exclusive_lease_and_is_audited_without_the_text() {
    let (lab, lead, w, w2) = setup().await;
    // No policy and no lease: no screenshot.
    lab.td
        .post_as(&w, "/api/targets/terra-win/display/screenshot", json!({}))
        .await
        .err(403, "forbidden");

    // A shared lease: the worker reads the screen, but sends no input.
    let g = lab
        .td
        .post_as(
            &lead,
            "/api/targets/terra-win/leases",
            json!({"agent": "worker"}),
        )
        .await;
    assert_eq!(g.status, 201, "{}", g.body);
    let r = lab
        .td
        .post_as(
            &w,
            "/api/targets/terra-win/display/screenshot?format=json",
            json!({}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let r = lab
        .td
        .post_as(
            &w,
            "/api/targets/terra-win/display/click",
            json!({"x": 10, "y": 20}),
        )
        .await;
    r.err(409, "conflict");
    let msg = r.body["error"]["message"].as_str().unwrap();
    assert!(msg.contains("--exclusive"), "{msg}");
    let info = lab
        .td
        .get_as(&w, "/api/targets/terra-win/display/info")
        .await;
    assert_eq!(info.ok()["input"]["allowed"], false);

    // An exclusive lease: input works.
    lab.td
        .req(
            hyper::Method::DELETE,
            "/api/targets/terra-win/leases/worker",
            None,
            Some(&lead),
        )
        .await
        .ok();
    let g = lab
        .td
        .post_as(
            &lead,
            "/api/targets/terra-win/leases",
            json!({"agent": "worker", "exclusive": true}),
        )
        .await;
    assert_eq!(g.status, 201, "{}", g.body);
    let r = lab
        .td
        .post_as(
            &w,
            "/api/targets/terra-win/display/click",
            json!({"x": 10, "y": 20, "button": "right"}),
        )
        .await;
    assert_eq!(r.ok()["ok"], true);
    assert!(
        lab.log()
            .contains("vmlab vm click --button right win11 10 20"),
        "{}",
        lab.log()
    );
    let r = lab
        .td
        .post_as(
            &w,
            "/api/targets/terra-win/display/type",
            json!({"text": "hunter2"}),
        )
        .await;
    assert_eq!(r.ok()["chars"], 7);
    let r = lab
        .td
        .post_as(
            &w,
            "/api/targets/terra-win/display/keys",
            json!({"chord": "Ctrl+Alt+Delete"}),
        )
        .await;
    assert_eq!(r.ok()["chord"], "ctrl-alt-delete");

    // The audit rows: one for each input, the length of the text only.
    let audit = lab.td.get("/api/audit?action=target.display").await;
    let rows = audit.ok().as_array().unwrap().clone();
    assert_eq!(rows.len(), 3, "{rows:?}");
    let all = serde_json::to_string(&rows).unwrap();
    assert!(!all.contains("hunter2"), "{all}");
    assert!(all.contains("\"chars\":7"), "{all}");
    assert!(all.contains("\"x\":10"), "{all}");

    // While the worker holds it, the lead (an admin) and a second worker
    // get `lease_held`.
    for who in [&lead, &w2] {
        let r = lab
            .td
            .post_as(
                who,
                "/api/targets/terra-win/display/move",
                json!({"x": 1, "y": 1}),
            )
            .await;
        if who == &w2 {
            // w2 may not even read the screen.
            r.err(403, "forbidden");
        } else {
            r.err(409, "lease_held");
        }
    }
    // The operator may always send input.
    lab.td
        .post(
            "/api/targets/terra-win/display/move",
            json!({"x": 1, "y": 1}),
        )
        .await
        .ok();
}

#[tokio::test]
async fn input_is_rate_limited_on_each_target() {
    let (lab, ..) = setup().await;
    let limiter = &lab
        .td
        .daemon
        .as_ref()
        .unwrap()
        .state
        .executor
        .display_limiter;
    let start = std::time::Instant::now();
    // Host subprocesses and HTTP requests can take more than a second under
    // load. Keep all requests in one rate window regardless of wall time.
    limiter.set_test_time(start);
    let calls = (0..30).map(|i| {
        lab.td.post(
            "/api/targets/terra-win/display/move",
            json!({"x": i, "y": i}),
        )
    });
    let answers = futures_util::future::join_all(calls).await;
    let limited = answers.iter().filter(|r| r.status == 429).count();
    let ok = answers.iter().filter(|r| r.status == 200).count();
    assert_eq!(limited, 10, "{limited} limited, {ok} ok");
    assert_eq!(ok, 20, "{ok} ok");
    // Screenshots are reads: no limit.
    lab.td
        .post("/api/targets/terra-win/display/screenshot", json!({}))
        .await
        .ok();
    limiter.set_test_time(start + Duration::from_millis(999));
    lab.td
        .post(
            "/api/targets/terra-win/display/move",
            json!({"x": 1, "y": 1}),
        )
        .await
        .err(429, "rate_limited");
    limiter.set_test_time(start + Duration::from_secs(1));
    lab.td
        .post(
            "/api/targets/terra-win/display/move",
            json!({"x": 1, "y": 1}),
        )
        .await
        .ok();
}

/// A one-connection RFB 3.8 server: VNC authentication with `password`,
/// a 4x3 screen, and a Raw update of blue pixels for each request. It
/// counts the key events.
async fn rfb_server(
    password: &'static str,
) -> (String, std::sync::Arc<std::sync::atomic::AtomicU32>) {
    use std::sync::atomic::Ordering;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let keys = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counter = keys.clone();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let (mut s, _) = l.accept().await.unwrap();
        s.write_all(b"RFB 003.008\n").await.unwrap();
        let mut v = [0u8; 12];
        s.read_exact(&mut v).await.unwrap();
        s.write_all(&[1, 2]).await.unwrap();
        assert_eq!(s.read_u8().await.unwrap(), 2);
        let challenge = *b"terrarium-chall!";
        s.write_all(&challenge).await.unwrap();
        let mut resp = [0u8; 16];
        s.read_exact(&mut resp).await.unwrap();
        let want = terrarium_host::display::vnc::vnc_auth_response(password.as_bytes(), &challenge);
        assert_eq!(resp, want, "the host agent got the credential's password");
        s.write_u32(0).await.unwrap();
        s.read_u8().await.unwrap();
        s.write_all(&[0, 4, 0, 3]).await.unwrap();
        s.write_all(&[32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0])
            .await
            .unwrap();
        s.write_u32(0).await.unwrap();
        loop {
            let Ok(t) = s.read_u8().await else {
                return;
            };
            let skip = match t {
                0 => 19,
                2 => {
                    s.read_u8().await.unwrap();
                    let n = s.read_u16().await.unwrap();
                    usize::from(n) * 4
                }
                3 => {
                    let mut m = [0u8; 9];
                    s.read_exact(&mut m).await.unwrap();
                    let mut out = vec![0u8, 0, 0, 1, 0, 0, 0, 0, 0, 4, 0, 3, 0, 0, 0, 0];
                    for _ in 0..12 {
                        out.extend_from_slice(&[255, 0, 0, 0]);
                    }
                    s.write_all(&out).await.unwrap();
                    0
                }
                4 => {
                    counter.fetch_add(1, Ordering::SeqCst);
                    7
                }
                5 => 5,
                t => panic!("client message {t}"),
            };
            let mut b = vec![0u8; skip];
            s.read_exact(&mut b).await.unwrap();
        }
    });
    (addr, keys)
}

#[tokio::test]
async fn vnc_and_rdp_targets_keep_their_secret_and_reach_the_server() {
    let (lab, ..) = setup().await;
    let r = lab
        .td
        .post(
            "/api/credentials",
            json!({"name": "vncpw", "kind": "password", "value": "s3cret pass", "username": "admin"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert!(!r.body.to_string().contains("s3cret"), "{}", r.body);

    let (vnc_addr, vnc_keys) = rfb_server("s3cret pass").await;
    lab.target(
        json!({"name": "terra-vnc", "kind": "vnc", "host": "lab1", "project": "terra",
                      "address": vnc_addr, "credential": "vncpw"}),
    )
    .await;
    // Port 1 on loopback refuses at once.
    lab.target(
        json!({"name": "terra-rdp", "kind": "rdp", "host": "lab1", "project": "terra",
                      "address": "127.0.0.1:1", "username": "Bob Smith", "domain": "LAB",
                      "credential": "vncpw"}),
    )
    .await;
    for path in [
        "/api/targets/terra-vnc",
        "/api/targets",
        "/api/credentials",
        "/api/credentials/vncpw",
    ] {
        let b = lab.td.get(path).await.body.to_string();
        assert!(!b.contains("s3cret"), "{path}: {b}");
    }
    let v = lab.td.get("/api/targets/terra-vnc").await;
    assert_eq!(v.ok()["has_display"], true);

    // VNC: the host agent logs in with the credential and reads the screen.
    let r = lab
        .td
        .post("/api/targets/terra-vnc/display/screenshot", json!({}))
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.headers["content-type"], "image/png");
    assert_eq!(r.headers["x-display-width"], "4");
    assert_eq!(r.headers["x-screen-height"], "3");
    let r = lab
        .td
        .post(
            "/api/targets/terra-vnc/display/keys",
            json!({"chord": "ctrl-l"}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    // The next screenshot is a round trip on the same connection: the
    // keys are in by then (ctrl down, l down, l up, ctrl up).
    let r = lab
        .td
        .post("/api/targets/terra-vnc/display/screenshot", json!({}))
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(vnc_keys.load(std::sync::atomic::Ordering::SeqCst), 4);

    // RDP: no server, so `failed`, with no secret in the message.
    let r = lab
        .td
        .post("/api/targets/terra-rdp/display/screenshot", json!({}))
        .await;
    r.err(422, "invalid");
    assert_eq!(r.body["error"]["details"]["code"], "failed");
    let m = r.body["error"]["message"].as_str().unwrap();
    assert!(m.contains("rdp 127.0.0.1:1: connect"), "{m}");
    assert!(!r.body.to_string().contains("s3cret"));

    for t in ["terra-vnc", "terra-rdp"] {
        // No exec on a display-only target.
        let r = lab
            .td
            .post(&format!("/api/targets/{t}/exec"), json!({"cmd": ["true"]}))
            .await;
        r.err(422, "invalid");
        assert!(r.body.to_string().contains("only a display"), "{}", r.body);
    }

    // Bad definitions.
    for (def, why) in [
        (
            json!({"name": "v1", "kind": "vnc", "host": "lab1"}),
            "address",
        ),
        (
            json!({"name": "v2", "kind": "vnc", "host": "lab1", "address": "h:5901", "port": 5902}),
            "not in both",
        ),
        (
            json!({"name": "v3", "kind": "vnc", "host": "lab1", "address": "h", "domain": "D"}),
            "domain",
        ),
        (
            json!({"name": "d1", "kind": "docker", "host": "lab1", "image": "a", "display_name": ":1"}),
            "display_name",
        ),
        (
            json!({"name": "h1", "kind": "host", "host": "lab1", "display": "x11"}),
            "display",
        ),
    ] {
        let r = lab.td.post("/api/targets", def).await;
        r.err(422, "invalid");
        assert!(r.body.to_string().contains(why), "{why}: {}", r.body);
    }
}

#[tokio::test]
async fn vnc_status_is_a_tcp_probe_from_the_host_agent() {
    let (lab, ..) = setup().await;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    lab.target(json!({"name": "probe-vnc", "kind": "vnc", "host": "lab1", "address": addr}))
        .await;
    let r = lab.td.post_empty("/api/targets/probe-vnc/status").await;
    let r = r.ok();
    assert_eq!(r["state"], "running", "{r}");
    assert_eq!(r["result"]["reachable"], true);
    drop(l);
    // Port 1 on loopback refuses at once (a freed port can be reused).
    lab.target(
        json!({"name": "closed-vnc", "kind": "vnc", "host": "lab1", "address": "127.0.0.1:1"}),
    )
    .await;
    let r = lab.td.post_empty("/api/targets/closed-vnc/status").await;
    assert_eq!(r.ok()["state"], "unknown");
    // up and down change nothing on the target.
    lab.td.post_empty("/api/targets/probe-vnc/up").await.ok();
}

#[tokio::test]
async fn the_desktop_needs_the_host_agents_own_opt_in() {
    let (lab, ..) = setup().await;
    lab.host("lab2", true).await;
    let h1 = lab.td.get("/api/hosts/lab1").await;
    assert_eq!(h1.ok()["capabilities"]["display"]["enabled"], false);
    assert_eq!(h1.ok()["summary"]["display"], false);
    let h2 = lab.td.get("/api/hosts/lab2").await;
    assert_eq!(h2.ok()["capabilities"]["display"]["enabled"], true);
    assert_eq!(h2.ok()["summary"]["display"], true);

    lab.target(json!({"name": "desk1", "kind": "desktop", "host": "lab1"}))
        .await;
    let r = lab
        .td
        .post("/api/targets/desk1/display/screenshot", json!({}))
        .await;
    r.err(403, "forbidden");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("[display] enabled = true"),
        "{}",
        r.body
    );
    let s = lab.td.post_empty("/api/targets/desk1/status").await;
    assert_eq!(s.ok()["state"], "unknown");
    lab.target(json!({"name": "desk2", "kind": "desktop", "host": "lab2"}))
        .await;
    let s = lab.td.post_empty("/api/targets/desk2/status").await;
    assert_eq!(s.ok()["state"], "running");
}

#[tokio::test]
async fn docker_x11_runs_the_tools_through_docker_exec() {
    let (lab, ..) = setup().await;
    lab.target(
        json!({"name": "desk", "kind": "docker", "host": "lab1", "container": "xdesk",
                      "display": "x11", "display_name": ":1"}),
    )
    .await;
    let r = lab
        .td
        .post("/api/targets/desk/display/screenshot", json!({}))
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    lab.td
        .post(
            "/api/targets/desk/display/click",
            json!({"x": 5, "y": 6, "double": true}),
        )
        .await
        .ok();
    let log = lab.log();
    assert!(
        log.contains("docker exec -e DISPLAY=:1 xdesk sh -c"),
        "{log}"
    );
    assert!(
        log.contains("docker exec -e DISPLAY=:1 xdesk xdotool mousemove 5 6 click --repeat 2 1"),
        "{log}"
    );
    // A docker target without `display` has none.
    lab.target(json!({"name": "plain", "kind": "docker", "host": "lab1", "image": "alpine"}))
        .await;
    let r = lab
        .td
        .post("/api/targets/plain/display/screenshot", json!({}))
        .await;
    r.err(422, "invalid");
    assert!(r.body.to_string().contains("has no display"), "{}", r.body);
}
