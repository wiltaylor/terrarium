//! The `exedev` target kind (TER-294) through the daemon's API: a fake
//! exe.dev HTTPS API and a fake `ssh`. `up` makes a VM with the tag and
//! the size, exec goes over ssh to `exedev@<ssh_dest>`, a second `up`
//! adopts the recorded VM, the fourth VM is refused, `down` removes the
//! VM, and the sweep removes a VM past its life (with a mail), an unheld
//! VM, and a leftover VM with the prefix and the tag, but not other VMs.
//!
//! `live_round_trip_on_exe_dev` (ignored) does the same on the real
//! exe.dev account: it needs `TERRARIUM_EXEDEV_LIVE_TOKEN` (the API
//! token) and `TERRARIUM_EXEDEV_LIVE_KEY` (the ssh private key), and it
//! removes each VM it makes.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};

use crate::common::TestDaemon;

const TOKEN: &str = "exe1.fixture-s3cret-token";

/// The VMs of the fake account (name to tags), and each command it got.
#[derive(Default)]
struct Fake {
    vms: BTreeMap<String, Vec<String>>,
    commands: Vec<String>,
}

async fn fake_api() -> (String, Arc<Mutex<Fake>>) {
    use axum::{Router, http::HeaderMap, http::StatusCode, routing::post};
    let fake = Arc::new(Mutex::new(Fake::default()));
    let f = fake.clone();
    let app = Router::new().route(
        "/exec",
        post(move |h: HeaderMap, body: String| {
            let f = f.clone();
            async move {
                let auth = h.get("authorization").and_then(|v| v.to_str().ok());
                if auth != Some(&format!("Bearer {TOKEN}")) {
                    return (StatusCode::UNAUTHORIZED, "bad token".to_owned());
                }
                let mut f = f.lock().unwrap();
                f.commands.push(body.clone());
                let words: Vec<&str> = body.split_whitespace().collect();
                match words.first().copied() {
                    Some("new") => {
                        let arg = |k: &str| {
                            words
                                .iter()
                                .find_map(|w| w.strip_prefix(&format!("--{k}=")))
                                .unwrap_or("")
                                .to_owned()
                        };
                        let name = arg("name");
                        f.vms.insert(name.clone(), vec![arg("tag")]);
                        let v = json!({"vm_name": name, "tags": [arg("tag")],
                                       "ssh_dest": format!("{name}.exe.xyz"), "ssh_port": 22});
                        (StatusCode::OK, v.to_string())
                    }
                    Some("ls") => {
                        let vms: Vec<Value> = f
                            .vms
                            .iter()
                            .map(|(n, t)| json!({"vm_name": n, "tags": t, "status": "running"}))
                            .collect();
                        (StatusCode::OK, json!({ "vms": vms }).to_string())
                    }
                    Some("rm") => {
                        let (mut deleted, mut not_found) = (Vec::new(), Vec::new());
                        for n in words[1..].iter().filter(|w| !w.starts_with("--")) {
                            if f.vms.remove(*n).is_some() {
                                deleted.push(*n);
                            } else {
                                not_found.push(*n);
                            }
                        }
                        let v = json!({"deleted": deleted, "not_found": not_found,
                                       "failed": [], "errors": {}});
                        (StatusCode::OK, v.to_string())
                    }
                    _ => (StatusCode::NOT_FOUND, "unknown command".to_owned()),
                }
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/exec", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, fake)
}

/// A fake `ssh`: it logs the destination and runs the remote command here.
fn fake_ssh(dir: &std::path::Path) -> (String, std::path::PathBuf) {
    let log = dir.join("ssh.log");
    let bin = dir.join("fake-ssh");
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\nfor a; do last=\"$a\"; done\nprev=\"\"\nfor a; do [ \"$prev\" = \"--\" ] && echo \"$a\" >> '{}'; prev=\"$a\"; done\nexec sh -c \"$last\"\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    (bin.display().to_string(), log)
}

async fn output(td: &TestDaemon, target: &str, sid: &str) -> (String, Value) {
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut out = Vec::new();
    let mut since = 0u64;
    for _ in 0..200 {
        let path = format!("/api/targets/{target}/sessions/{sid}/output?since={since}&wait=5");
        let o = td.get(&path).await.ok().clone();
        for c in o["chunks"].as_array().unwrap() {
            if c["stream"] == "stdout" {
                out.extend(b64.decode(c["data"].as_str().unwrap()).unwrap());
            }
        }
        since = o["next"].as_u64().unwrap();
        if o["done"] == true {
            return (String::from_utf8_lossy(&out).into_owned(), o["end"].clone());
        }
    }
    panic!("session {sid} never ended");
}

async fn secrets(td: &TestDaemon, token: &str, key: Option<&str>) {
    let r = td
        .post(
            "/api/secrets",
            json!({"name": "exedev-api-token", "kind": "api_key", "fields": {"value": token}}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 201, "{}", r.body);
    let r = match key {
        None => {
            td.post("/api/secrets/global/vmkey/generate", json!({}))
                .await
        }
        Some(k) => {
            td.put(
                "/api/secrets/global/vmkey",
                json!({"kind": "ssh_key", "fields": {"private_key": k}}),
            )
            .await
        }
    };
    assert_eq!(r.status.as_u16(), 201, "{}", r.body);
}

fn target(name: &str, life: u64) -> Value {
    json!({"name": name, "kind": "exedev", "ephemeral": true, "credential": "vmkey",
           "exedev": {"max_life_secs": life}})
}

/// Move the times of a target's recorded VM `secs` into the past.
fn age(td: &TestDaemon, target: &str, secs: i64) {
    let conn = terrariumd::db::open(&td.state.join("terrarium.db")).unwrap();
    let rt: String = conn
        .query_row(
            "SELECT runtime FROM targets WHERE name = ?1",
            [target],
            |r| r.get(0),
        )
        .unwrap();
    let mut rt: Value = serde_json::from_str(&rt).unwrap();
    for k in ["created_at", "expires_at"] {
        let t = chrono::DateTime::parse_from_rfc3339(rt["exedev"][k].as_str().unwrap()).unwrap();
        rt["exedev"][k] = (t - chrono::Duration::seconds(secs)).to_rfc3339().into();
    }
    conn.execute(
        "UPDATE targets SET runtime = ?2 WHERE name = ?1",
        rusqlite::params![target, rt.to_string()],
    )
    .unwrap();
}

fn vm_of(td: &TestDaemon, target: &str) -> Option<String> {
    let conn = terrariumd::db::open(&td.state.join("terrarium.db")).unwrap();
    let rt: String = conn
        .query_row(
            "SELECT runtime FROM targets WHERE name = ?1",
            [target],
            |r| r.get(0),
        )
        .unwrap();
    let rt: Value = serde_json::from_str(&rt).unwrap();
    rt["exedev"]["name"].as_str().map(str::to_owned)
}

#[tokio::test]
async fn exedev_targets_make_use_cap_and_sweep_vms_through_the_api() {
    let (url, fake) = fake_api().await;
    let mut td = TestDaemon::start().await;
    let (ssh, ssh_log) = fake_ssh(td.dir.path());
    td.boot_with_executor(None, None, |x| {
        x.exedev_api = url.clone();
        x.ssh_bin = ssh.clone();
    })
    .await;
    secrets(&td, TOKEN, None).await;
    for t in ["ex1", "ex2", "ex3", "ex4"] {
        let life = if t == "ex1" { 60 } else { 8 * 3600 };
        let r = td.post("/api/targets", target(t, life)).await;
        assert_eq!(r.status.as_u16(), 201, "{}", r.body);
    }
    // The rules of the kind.
    let r = td
        .post(
            "/api/targets",
            json!({"name": "bad", "kind": "exedev", "ephemeral": true, "credential": "vmkey",
                   "exedev": {"max_life_secs": 9 * 3600}}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 422, "{}", r.body);

    // `up` makes a VM with the tag and the size; exec runs over ssh on it.
    let r = td.post_empty("/api/targets/ex1/up").await;
    assert_eq!(r.ok()["state"], "running", "{}", r.body);
    let vm1 = vm_of(&td, "ex1").unwrap();
    assert!(vm1.starts_with("terrarium-x-ex1-"), "{vm1}");
    {
        let f = fake.lock().unwrap();
        assert_eq!(f.vms[&vm1], vec!["terrarium-dev-ssh".to_owned()]);
        let new = f.commands.iter().find(|c| c.starts_with("new ")).unwrap();
        assert!(
            new.contains("--sandbox") && new.contains("--cpu=2") && new.contains("--memory=4GB"),
            "{new}"
        );
        assert!(f.commands.iter().all(|c| !c.contains("s3cret")));
    }
    let r = td
        .post(
            "/api/targets/ex1/exec",
            json!({"cmd": ["sh", "-c", "echo on-the-vm"]}),
        )
        .await;
    let sid = r.ok()["session_id"].as_str().unwrap().to_owned();
    let (out, end) = output(&td, "ex1", &sid).await;
    assert_eq!(out.trim(), "on-the-vm", "{end}");
    let log = std::fs::read_to_string(&ssh_log).unwrap();
    assert!(log.contains(&format!("exedev@{vm1}.exe.xyz")), "{log}");

    // A second `up` (a restart, a lease grant) adopts the recorded VM.
    td.post_empty("/api/targets/ex1/up").await.ok();
    assert_eq!(vm_of(&td, "ex1").as_deref(), Some(vm1.as_str()));
    assert_eq!(fake.lock().unwrap().vms.len(), 1);

    // At most 3 VMs at once.
    td.post_empty("/api/targets/ex2/up").await.ok();
    td.post_empty("/api/targets/ex3/up").await.ok();
    let r = td.post_empty("/api/targets/ex4/up").await;
    assert_eq!(r.status.as_u16(), 409, "{}", r.body);
    let msg = r.body["error"]["message"].as_str().unwrap();
    assert!(msg.contains("exe.dev cap: 3 of 3"), "{msg}");
    assert_eq!(fake.lock().unwrap().vms.len(), 3);
    assert!(vm_of(&td, "ex4").is_none());

    // `down` removes the VM.
    let r = td.post_empty("/api/targets/ex3/down").await;
    assert_eq!(r.ok()["state"], "absent", "{}", r.body);
    assert!(vm_of(&td, "ex3").is_none());
    assert_eq!(fake.lock().unwrap().vms.len(), 2);

    // The sweep: ex1 is past its life, ex2 has no holder for an hour, one
    // VM is left by no target. A VM without the prefix or the tag stays.
    age(&td, "ex1", 120);
    age(&td, "ex2", 3600);
    {
        let mut f = fake.lock().unwrap();
        f.vms.insert(
            "terrarium-x-lost-1".into(),
            vec!["terrarium-dev-ssh".into()],
        );
        f.vms
            .insert("personal".into(), vec!["terrarium-dev-ssh".into()]);
        f.vms
            .insert("terrarium-x-other-1".into(), vec!["other".into()]);
    }
    td.project("p").await;
    let agent = td.agent("p", "a", json!({})).await;
    let id = agent["id"].as_str().unwrap();
    td.post_empty_as(id, "/api/exedev/sweep")
        .await
        .err(403, "forbidden");
    let r = td.post_empty("/api/exedev/sweep").await;
    let removed: Vec<(String, String)> = r.ok()["removed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| {
            (
                x["vm"].as_str().unwrap().to_owned(),
                x["reason"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let vm2 = removed[1].0.clone();
    assert_eq!(
        removed,
        vec![
            (vm1.clone(), "expired".to_owned()),
            (vm2, "unheld".to_owned()),
            ("terrarium-x-lost-1".to_owned(), "leftover".to_owned()),
        ],
        "{}",
        r.body
    );
    assert!(
        r.body["errors"].as_array().unwrap().is_empty(),
        "{}",
        r.body
    );
    let left: Vec<String> = fake.lock().unwrap().vms.keys().cloned().collect();
    assert_eq!(left, vec!["personal", "terrarium-x-other-1"]);
    assert!(vm_of(&td, "ex1").is_none() && vm_of(&td, "ex2").is_none());
    assert_eq!(td.get("/api/targets/ex1").await.ok()["state"], "absent");
    // The VM past its life is reported to the operator (a global target).
    let conn = terrariumd::db::open(&td.state.join("terrarium.db")).unwrap();
    let subjects: Vec<String> = conn
        .prepare("SELECT subject FROM messages WHERE subject LIKE 'exe.dev VM %'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        subjects,
        vec![format!("exe.dev VM {vm1} removed: past its life")]
    );
    // Deleting a target removes its VM.
    td.post_empty("/api/targets/ex4/up").await.ok();
    assert!(vm_of(&td, "ex4").is_some());
    td.delete("/api/targets/ex4").await.ok();
    assert_eq!(fake.lock().unwrap().vms.len(), 2);
}

/// The live round trip on the operator's exe.dev account (TER-294
/// acceptance): make one VM through the kind, run a command on it over
/// ssh, let the sweep remove it past a short life, and show with `ls` that
/// no VM of the run is left. It removes each VM it made, also on a failure.
#[tokio::test]
#[ignore = "live: needs TERRARIUM_EXEDEV_LIVE_TOKEN and TERRARIUM_EXEDEV_LIVE_KEY"]
async fn live_round_trip_on_exe_dev() {
    let token = std::env::var("TERRARIUM_EXEDEV_LIVE_TOKEN").expect("the API token");
    let key = std::env::var("TERRARIUM_EXEDEV_LIVE_KEY").expect("the ssh private key");
    let api = terrariumd::exedev::Api::new(
        terrariumd::exedev::DEFAULT_API,
        zeroize::Zeroizing::new(token.clone()),
    )
    .unwrap();
    let prefix = "terrarium-x-live";
    let before = api.ls().await.unwrap();
    println!(
        "ls before: {:?}",
        before.iter().map(|l| &l.name).collect::<Vec<_>>()
    );
    let body = tokio::spawn(async move {
        let td = TestDaemon::start().await;
        secrets(&td, &token, Some(&key)).await;
        let mut t = target("live", 60);
        t["sync_workspace"] = false.into();
        td.post("/api/targets", t).await.ok();
        let r = td.post_empty("/api/targets/live/up").await;
        assert_eq!(r.ok()["state"], "running", "{}", r.body);
        let vm = vm_of(&td, "live").unwrap();
        println!("made VM {vm}");
        let r = td
            .post(
                "/api/targets/live/exec",
                json!({"cmd": ["sh", "-c", "uname -a; nproc; git --version"]}),
            )
            .await;
        let sid = r.ok()["session_id"].as_str().unwrap().to_owned();
        let (out, end) = output(&td, "live", &sid).await;
        println!("exec on the VM: {out}");
        assert!(
            out.contains("Linux") && out.contains("git version"),
            "{out} {end}"
        );
        // Past its life of 60 s, the sweep removes it.
        tokio::time::sleep(Duration::from_secs(65)).await;
        let r = td.post_empty("/api/exedev/sweep").await;
        println!("sweep: {}", r.body);
        let removed = &r.ok()["removed"];
        assert_eq!(removed[0]["vm"], vm.as_str(), "{removed}");
        assert_eq!(removed[0]["reason"], "expired", "{removed}");
        assert_eq!(td.get("/api/targets/live").await.ok()["state"], "absent");
        vm
    })
    .await;
    // Remove every VM of this run that is left, then report.
    let left: Vec<String> = api
        .ls()
        .await
        .unwrap()
        .into_iter()
        .map(|l| l.name)
        .filter(|n| n.starts_with(prefix))
        .collect();
    if !left.is_empty() {
        api.rm(&left).await.unwrap();
    }
    let after = api.ls().await.unwrap();
    println!(
        "ls after: {:?}",
        after.iter().map(|l| &l.name).collect::<Vec<_>>()
    );
    let vm = body.unwrap();
    assert!(left.is_empty(), "the sweep left {left:?}");
    assert!(
        after
            .iter()
            .all(|l| l.name != vm && !l.name.starts_with(prefix))
    );
}
