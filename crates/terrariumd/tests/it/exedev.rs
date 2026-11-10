//! The `exedev` target kind (TER-294) through the daemon: a fake exe.dev
//! HTTPS API and a fake `ssh`.
//!
//! - `up` makes a VM with the tag and the size, exec goes over ssh to
//!   `exedev@<ssh_dest>`, a second `up` adopts the recorded VM, the fourth
//!   VM is refused, `down` removes the VM, and the sweep removes a VM past
//!   its life (with a mail), an unheld VM, and an old leftover VM.
//! - The hooks: an agent start makes the VM and its stop removes it; a
//!   lease grant makes it and the lease end removes it.
//! - Two concurrent `up` calls make one VM. A record written before `new`
//!   is adopted after a restart. A failed `new` removes the VM. A target
//!   deleted while `rm` fails leaves a record that the sweep acts on. Two
//!   accounts of one exe.dev account do not remove each other's VMs.
//!
//! `live_round_trip_on_exe_dev` (ignored) runs on the real exe.dev
//! account: it needs `EXEDEV_LIVE_TOKEN` (the API token) and
//! `EXEDEV_LIVE_SSH_KEY_FILE` (a file with the ssh private key; or the key
//! itself in `EXEDEV_LIVE_SSH_KEY`), and it removes each VM it makes.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};

use crate::common::TestDaemon;

const TOKEN: &str = "exe1.fixture-s3cret-token";

/// The fake account: its VMs (name to tags and creation time), each
/// command it got, and the faults to inject.
#[derive(Default)]
struct Fake {
    vms: BTreeMap<String, (Vec<String>, chrono::DateTime<chrono::Utc>)>,
    commands: Vec<String>,
    /// `new` makes the VM and then answers 504.
    fail_new: bool,
    /// `rm` answers 500 and removes nothing.
    fail_rm: bool,
    /// `new` waits this long before it answers.
    new_delay_ms: u64,
    /// `ls` reports each VM this much older than it is.
    ls_older_secs: i64,
}

impl Fake {
    fn add(&mut self, name: &str, tag: &str, age_secs: i64) {
        self.vms.insert(
            name.to_owned(),
            (
                vec![tag.to_owned()],
                chrono::Utc::now() - chrono::Duration::seconds(age_secs),
            ),
        );
    }

    fn names(&self) -> Vec<String> {
        self.vms.keys().cloned().collect()
    }

    fn news(&self) -> usize {
        self.commands
            .iter()
            .filter(|c| c.starts_with("new "))
            .count()
    }
}

type FakeRef = Arc<Mutex<Fake>>;

async fn fake_api() -> (String, FakeRef) {
    use axum::{Router, http::HeaderMap, http::StatusCode, routing::post};
    let fake: FakeRef = Arc::default();
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
                let words: Vec<String> = body.split_whitespace().map(str::to_owned).collect();
                let delay = {
                    let mut g = f.lock().unwrap();
                    g.commands.push(body.clone());
                    if words.first().map(String::as_str) == Some("new") {
                        g.new_delay_ms
                    } else {
                        0
                    }
                };
                tokio::time::sleep(Duration::from_millis(delay)).await;
                let mut g = f.lock().unwrap();
                match words.first().map(String::as_str) {
                    Some("new") => {
                        let arg = |k: &str| {
                            words
                                .iter()
                                .find_map(|w| w.strip_prefix(&format!("--{k}=")))
                                .unwrap_or("")
                                .to_owned()
                        };
                        let name = arg("name");
                        g.add(&name, &arg("tag"), 0);
                        if g.fail_new {
                            return (StatusCode::GATEWAY_TIMEOUT, "timeout".to_owned());
                        }
                        let v = json!({"vm_name": name, "tags": [arg("tag")],
                                       "ssh_dest": format!("{name}.exe.xyz"), "ssh_port": 22});
                        (StatusCode::OK, v.to_string())
                    }
                    Some("ls") => {
                        let older = chrono::Duration::seconds(g.ls_older_secs);
                        let vms: Vec<Value> = g
                            .vms
                            .iter()
                            .map(|(n, (t, at))| {
                                json!({"vm_name": n, "tags": t, "status": "running",
                                       "ssh_dest": format!("{n}.exe.xyz"),
                                       "created_at": (*at - older).to_rfc3339()})
                            })
                            .collect();
                        (StatusCode::OK, json!({ "vms": vms }).to_string())
                    }
                    Some("rm") => {
                        if g.fail_rm {
                            return (StatusCode::INTERNAL_SERVER_ERROR, "busy".to_owned());
                        }
                        let (mut deleted, mut not_found) = (Vec::new(), Vec::new());
                        for n in words[1..].iter().filter(|w| !w.starts_with("--")) {
                            if g.vms.remove(n).is_some() {
                                deleted.push(n.clone());
                            } else {
                                not_found.push(n.clone());
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

/// A daemon on the fake API and the fake ssh, with the secrets.
async fn daemon() -> (TestDaemon, FakeRef, std::path::PathBuf, String, String) {
    let (url, fake) = fake_api().await;
    let mut td = TestDaemon::start().await;
    let (ssh, ssh_log) = fake_ssh(td.dir.path());
    reboot(&mut td, &url, &ssh).await;
    secrets(&td, TOKEN, None).await;
    (td, fake, ssh_log, url, ssh)
}

async fn reboot(td: &mut TestDaemon, url: &str, ssh: &str) {
    td.boot_with_executor(None, None, |x| {
        x.exedev_api = url.to_owned();
        x.ssh_bin = ssh.to_owned();
    })
    .await;
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

fn db(td: &TestDaemon) -> rusqlite::Connection {
    terrariumd::db::open(&td.state.join("terrarium.db")).unwrap()
}

/// Move the times of a target's recorded VM `secs` into the past.
fn age(td: &TestDaemon, target: &str, secs: i64) {
    let conn = db(td);
    let (c, e): (String, String) = conn
        .query_row(
            "SELECT created_at, expires_at FROM exedev_vms WHERE target = ?1",
            [target],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let back = |s: &str| {
        (chrono::DateTime::parse_from_rfc3339(s).unwrap() - chrono::Duration::seconds(secs))
            .to_rfc3339()
    };
    conn.execute(
        "UPDATE exedev_vms SET created_at = ?2, expires_at = ?3 WHERE target = ?1",
        rusqlite::params![target, back(&c), back(&e)],
    )
    .unwrap();
}

/// The recorded VM of a target, and its ssh host.
fn record(td: &TestDaemon, target: &str) -> Option<(String, Option<String>)> {
    use rusqlite::OptionalExtension;
    db(td)
        .query_row(
            "SELECT name, ssh_dest FROM exedev_vms WHERE target = ?1",
            [target],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .unwrap()
}

fn vm_of(td: &TestDaemon, target: &str) -> Option<String> {
    record(td, target).map(|r| r.0)
}

/// The `target.destroy` audit rows of a target: `(vm, reason)`.
fn destroyed(td: &TestDaemon, target: &str) -> Vec<(String, String)> {
    let conn = db(td);
    let rows: Vec<String> = conn
        .prepare("SELECT detail FROM audit WHERE action = 'target.destroy' AND target = ?1")
        .unwrap()
        .query_map([target], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    rows.iter()
        .map(|d| {
            let v: Value = serde_json::from_str(d).unwrap();
            (
                v["vm"].as_str().unwrap_or("").to_owned(),
                v["reason"].as_str().unwrap_or("").to_owned(),
            )
        })
        .collect()
}

/// `(vm, reason)` of each removal in a sweep answer.
async fn sweep(td: &TestDaemon) -> Vec<(String, String)> {
    let r = td.post_empty("/api/exedev/sweep").await;
    let body = r.ok().clone();
    assert!(body["errors"].as_array().unwrap().is_empty(), "{body}");
    body["removed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| {
            (
                x["vm"].as_str().unwrap().to_owned(),
                x["reason"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn state(td: &TestDaemon) -> terrariumd::api::Shared {
    td.daemon.as_ref().unwrap().state.clone()
}

async fn started_agent(td: &TestDaemon, project: &str, name: &str) -> String {
    let a = td.agent(project, name, json!({})).await;
    let id = a["id"].as_str().unwrap().to_owned();
    td.post_empty(&format!("/api/agents/{id}/start")).await.ok();
    td.wait_status(&id, "idle").await;
    id
}

#[tokio::test]
async fn exedev_targets_make_use_cap_and_sweep_vms_through_the_api() {
    let (td, fake, ssh_log, _, _) = daemon().await;
    for t in ["ex1", "ex2", "ex3", "ex4"] {
        let r = td.post("/api/targets", target(t, 8 * 3600)).await;
        assert_eq!(r.status.as_u16(), 201, "{}", r.body);
    }
    // The rules of the kind.
    let r = td.post("/api/targets", target("bad", 9 * 3600)).await;
    assert_eq!(r.status.as_u16(), 422, "{}", r.body);

    // `up` makes a VM with the tag and the size; exec runs over ssh on it.
    let r = td.post_empty("/api/targets/ex1/up").await;
    assert_eq!(r.ok()["state"], "running", "{}", r.body);
    let vm1 = vm_of(&td, "ex1").unwrap();
    assert!(vm1.starts_with("terrarium-x-ex1-"), "{vm1}");
    {
        let f = fake.lock().unwrap();
        assert_eq!(f.vms[&vm1].0, vec!["terrarium-dev-ssh".to_owned()]);
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

    // A second `up` adopts the recorded VM.
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

    // The sweep: ex1 is past its life, ex2 has had no holder for an hour,
    // one VM has been left by no target for an hour. A new leftover, and
    // VMs without the prefix or the tag, stay.
    age(&td, "ex1", 9 * 3600);
    age(&td, "ex2", 3600);
    {
        let mut f = fake.lock().unwrap();
        f.add("terrarium-x-lost-1", "terrarium-dev-ssh", 3600);
        f.add("terrarium-x-young-1", "terrarium-dev-ssh", 0);
        f.add("personal", "terrarium-dev-ssh", 3600);
        f.add("terrarium-x-other-1", "other", 3600);
    }
    td.project("p").await;
    let agent = td.agent("p", "a", json!({})).await;
    let id = agent["id"].as_str().unwrap();
    td.post_empty_as(id, "/api/exedev/sweep")
        .await
        .err(403, "forbidden");
    let removed = sweep(&td).await;
    let vm2 = removed[1].0.clone();
    assert_eq!(
        removed,
        vec![
            (vm1.clone(), "expired".to_owned()),
            (vm2, "unheld".to_owned()),
            ("terrarium-x-lost-1".to_owned(), "leftover".to_owned()),
        ]
    );
    assert_eq!(
        fake.lock().unwrap().names(),
        vec!["personal", "terrarium-x-other-1", "terrarium-x-young-1"]
    );
    assert!(vm_of(&td, "ex1").is_none() && vm_of(&td, "ex2").is_none());
    assert_eq!(td.get("/api/targets/ex1").await.ok()["state"], "absent");
    // The VM past its life is reported to the operator (a global target).
    let subjects: Vec<String> = db(&td)
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
    assert_eq!(
        destroyed(&td, "ex1"),
        vec![(vm1.clone(), "exedev_expired".to_owned())]
    );
    // Deleting a target removes its VM.
    td.post_empty("/api/targets/ex4/up").await.ok();
    let vm4 = vm_of(&td, "ex4").unwrap();
    td.delete("/api/targets/ex4").await.ok();
    assert!(!fake.lock().unwrap().vms.contains_key(&vm4));
    assert!(vm_of(&td, "ex4").is_none());
}

/// Finding 3: two `up` calls on one target at once make one VM, and both
/// get it.
#[tokio::test]
async fn two_concurrent_up_calls_make_one_vm() {
    let (td, fake, _, _, _) = daemon().await;
    td.post("/api/targets", target("ex", 8 * 3600)).await.ok();
    fake.lock().unwrap().new_delay_ms = 300;
    let (a, b) = tokio::join!(
        td.post_empty("/api/targets/ex/up"),
        td.post_empty("/api/targets/ex/up")
    );
    assert_eq!(a.ok()["state"], "running", "{}", a.body);
    assert_eq!(b.ok()["state"], "running", "{}", b.body);
    let f = fake.lock().unwrap();
    assert_eq!(f.news(), 1, "{:?}", f.commands);
    assert_eq!(f.names(), vec![vm_of(&td, "ex").unwrap()]);
}

/// Finding 4: an agent start makes the VM and the agent stop removes it
/// (`executor::on_agent_start` and `on_agent_stop`, as the supervisor
/// calls them).
#[tokio::test]
async fn an_agent_start_makes_the_vm_and_its_stop_removes_it() {
    use terrariumd::executor::{on_agent_start, on_agent_stop};
    let (td, fake, _, _, _) = daemon().await;
    td.project("p").await;
    let mut t = target("ex", 8 * 3600);
    t["project"] = "p".into();
    td.post("/api/targets", t).await.ok();
    let w = started_agent(&td, "p", "w").await;
    let errs = on_agent_start(&state(&td), &w, &["p-ex".into()]).await;
    assert!(errs.is_empty(), "{errs:?}");
    let vm = vm_of(&td, "p-ex").expect("the agent start made a VM");
    assert_eq!(fake.lock().unwrap().names(), vec![vm.clone()]);
    let errs = on_agent_stop(&state(&td), &w, &["p-ex".into()]).await;
    assert!(errs.is_empty(), "{errs:?}");
    assert!(
        fake.lock().unwrap().vms.is_empty(),
        "the agent stop removed it"
    );
    assert!(vm_of(&td, "p-ex").is_none());
    let reasons: Vec<String> = destroyed(&td, "p-ex").into_iter().map(|r| r.1).collect();
    assert_eq!(reasons, vec!["agent_stop".to_owned()]);
}

/// Finding 4: a lease grant to a running agent makes the VM and the lease
/// end removes it.
#[tokio::test]
async fn a_lease_grant_makes_the_vm_and_its_end_removes_it() {
    let (td, fake, _, _, _) = daemon().await;
    td.project("p").await;
    let mut t = target("ex", 8 * 3600);
    t["project"] = "p".into();
    td.post("/api/targets", t).await.ok();
    started_agent(&td, "p", "w").await;
    let r = td
        .post(
            "/api/targets/p-ex/leases",
            json!({"agent": "p/w", "until": "1h"}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 201, "{}", r.body);
    assert_eq!(r.body["lifecycle"]["state"], "running", "{}", r.body);
    let vm = vm_of(&td, "p-ex").expect("the lease grant made a VM");
    assert_eq!(fake.lock().unwrap().names(), vec![vm]);
    let r = td.delete("/api/targets/p-ex/leases/p%2Fw").await;
    assert_eq!(r.status.as_u16(), 200, "{}", r.body);
    assert!(
        fake.lock().unwrap().vms.is_empty(),
        "the lease end removed it"
    );
    assert!(vm_of(&td, "p-ex").is_none());
}

/// Finding 4: a record written before `new` answered (the daemon died) is
/// adopted after a restart: no second VM. Finding 5: a recorded VM past
/// its life is never adopted.
#[tokio::test]
async fn a_restart_adopts_a_vm_recorded_before_new_and_never_an_expired_one() {
    let (mut td, fake, _, url, ssh) = daemon().await;
    td.post("/api/targets", target("ex", 8 * 3600)).await.ok();
    let now = chrono::Utc::now();
    db(&td)
        .execute(
            "INSERT INTO exedev_vms (name, target, project, api_token, admin_set, tag,
                                     ssh_dest, created_at, expires_at)
             VALUES ('terrarium-x-ex-crash', 'ex', NULL, 'exedev-api-token', 1,
                     'terrarium-dev-ssh', NULL, ?1, ?2)",
            rusqlite::params![
                now.to_rfc3339(),
                (now + chrono::Duration::hours(8)).to_rfc3339()
            ],
        )
        .unwrap();
    fake.lock()
        .unwrap()
        .add("terrarium-x-ex-crash", "terrarium-dev-ssh", 0);
    reboot(&mut td, &url, &ssh).await;
    let r = td.post_empty("/api/targets/ex/up").await;
    assert_eq!(r.ok()["state"], "running", "{}", r.body);
    assert_eq!(
        record(&td, "ex"),
        Some((
            "terrarium-x-ex-crash".into(),
            Some("terrarium-x-ex-crash.exe.xyz".into())
        ))
    );
    assert_eq!(fake.lock().unwrap().news(), 0);
    // Past its life: removed and replaced, not adopted.
    age(&td, "ex", 9 * 3600);
    td.post_empty("/api/targets/ex/up").await.ok();
    let new = vm_of(&td, "ex").unwrap();
    assert_ne!(new, "terrarium-x-ex-crash");
    assert_eq!(fake.lock().unwrap().names(), vec![new]);
}

/// Finding 4: a `new` that fails after it made the VM removes the VM.
/// Finding 6: when that `rm` fails too, the record (no ssh host) stays,
/// and the sweep removes the VM after the grace time, held or not.
#[tokio::test]
async fn a_failed_new_removes_its_vm_or_leaves_it_to_the_sweep() {
    let (td, fake, _, _, _) = daemon().await;
    td.post("/api/targets", target("ex", 8 * 3600)).await.ok();
    fake.lock().unwrap().fail_new = true;
    let r = td.post_empty("/api/targets/ex/up").await;
    assert_eq!(r.status.as_u16(), 500, "{}", r.body);
    assert!(fake.lock().unwrap().vms.is_empty());
    assert!(vm_of(&td, "ex").is_none());

    fake.lock().unwrap().fail_rm = true;
    let r = td.post_empty("/api/targets/ex/up").await;
    assert_eq!(r.status.as_u16(), 500, "{}", r.body);
    let (vm, dest) = record(&td, "ex").expect("the record stays");
    assert!(dest.is_none());
    {
        let mut f = fake.lock().unwrap();
        assert_eq!(f.names(), vec![vm.clone()]);
        f.fail_rm = false;
        f.fail_new = false;
    }
    // Young: kept. After the grace time: removed, also while an agent
    // holds the target.
    // A running agent holds the target by its policy (no `up` runs).
    td.project("p").await;
    let a = td
        .agent("p", "w", json!({"policy": {"targets": ["ex"]}}))
        .await;
    let w = a["id"].as_str().unwrap().to_owned();
    td.post_empty(&format!("/api/agents/{w}/start")).await.ok();
    td.wait_status(&w, "idle").await;
    assert!(sweep(&td).await.is_empty());
    age(&td, "ex", 3600);
    assert_eq!(sweep(&td).await, vec![(vm, "unready".to_owned())]);
    assert!(fake.lock().unwrap().vms.is_empty());
}

/// Finding 1: the last exedev target is deleted while `rm` fails. The VM's
/// record outlives the target, and the next sweep removes the VM.
#[tokio::test]
async fn a_vm_of_a_deleted_target_is_removed_by_the_sweep() {
    let (td, fake, _, _, _) = daemon().await;
    td.post("/api/targets", target("ex", 8 * 3600)).await.ok();
    td.post_empty("/api/targets/ex/up").await.ok();
    let vm = vm_of(&td, "ex").unwrap();
    fake.lock().unwrap().fail_rm = true;
    let r = td.delete("/api/targets/ex").await;
    assert!(r.status.is_success(), "{}", r.body);
    assert_eq!(td.get("/api/targets/ex").await.status.as_u16(), 404);
    assert_eq!(vm_of(&td, "ex").as_deref(), Some(vm.as_str()));
    assert_eq!(fake.lock().unwrap().names(), vec![vm.clone()]);
    fake.lock().unwrap().fail_rm = false;
    assert_eq!(sweep(&td).await, vec![(vm, "orphaned".to_owned())]);
    assert!(fake.lock().unwrap().vms.is_empty());
    assert!(vm_of(&td, "ex").is_none());
}

/// Finding 2: a global target and a project target that reach the same
/// exe.dev account with the same token are two accounts to the sweep.
/// Neither removes the other's VM as a leftover.
#[tokio::test]
async fn two_accounts_of_one_exe_dev_account_keep_each_others_vms() {
    let (td, fake, _, _, _) = daemon().await;
    td.project("p").await;
    td.post("/api/targets", target("g", 8 * 3600)).await.ok();
    let mut t = target("ex", 8 * 3600);
    t["project"] = "p".into();
    td.post("/api/targets", t).await.ok();
    td.post_empty("/api/targets/g/up").await.ok();
    td.post_empty("/api/targets/p-ex/up").await.ok();
    assert_eq!(fake.lock().unwrap().vms.len(), 2);
    // `ls` shows both as an hour old: past the leftover grace time.
    fake.lock().unwrap().ls_older_secs = 3600;
    assert!(sweep(&td).await.is_empty());
    assert_eq!(fake.lock().unwrap().vms.len(), 2);
}

/// The live round trip on the operator's exe.dev account (TER-294
/// acceptance): make one VM through the kind, check that `ls` lists it, run
/// a command on it over ssh, let the sweep timer remove it at the end of
/// its life, and show with `ls` that no VM of the run is left. It removes
/// each VM it made, also on a failure.
///
/// The life is [`terrariumd::exedev::FIRST_SWEEP`] plus 60 s, so the VM
/// outlives the daemon's first sweep. The timer then runs at its
/// `expires_at` and removes it as `expired`.
#[tokio::test]
#[ignore = "live: needs EXEDEV_LIVE_TOKEN and EXEDEV_LIVE_SSH_KEY_FILE"]
async fn live_round_trip_on_exe_dev() {
    let token = std::env::var("EXEDEV_LIVE_TOKEN").expect("EXEDEV_LIVE_TOKEN: the API token");
    let key = match std::env::var("EXEDEV_LIVE_SSH_KEY_FILE") {
        Ok(path) => std::fs::read_to_string(path).expect("the ssh key file"),
        Err(_) => std::env::var("EXEDEV_LIVE_SSH_KEY")
            .expect("EXEDEV_LIVE_SSH_KEY_FILE or EXEDEV_LIVE_SSH_KEY: the ssh private key"),
    };
    let api = terrariumd::exedev::Api::new(
        terrariumd::exedev::DEFAULT_API,
        zeroize::Zeroizing::new(token.clone()),
    )
    .unwrap();
    let prefix = "terrarium-x-live";
    let names = |l: &[terrariumd::exedev::Listed]| -> Vec<String> {
        l.iter().map(|x| x.name.clone()).collect()
    };
    println!("ls before: {:?}", names(&api.ls().await.unwrap()));
    let life = terrariumd::exedev::FIRST_SWEEP.as_secs() + 60;
    let body = tokio::spawn(async move {
        let api = terrariumd::exedev::Api::new(
            terrariumd::exedev::DEFAULT_API,
            zeroize::Zeroizing::new(token.clone()),
        )
        .unwrap();
        let td = TestDaemon::start().await;
        secrets(&td, &token, Some(&key)).await;
        td.post("/api/targets", target("live", life)).await.ok();
        let r = td.post_empty("/api/targets/live/up").await;
        assert_eq!(r.ok()["state"], "running", "{}", r.body);
        let up_at = std::time::Instant::now();
        let vm = vm_of(&td, "live").unwrap();
        let listed = names(&api.ls().await.unwrap());
        println!("made VM {vm}; ls after up: {listed:?}");
        assert!(listed.contains(&vm), "ls does not list {vm} after up");
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
        // Still there after the exec and the daemon's first sweep.
        let past_first = terrariumd::exedev::FIRST_SWEEP + Duration::from_secs(5);
        tokio::time::sleep(past_first.saturating_sub(up_at.elapsed())).await;
        let listed = names(&api.ls().await.unwrap());
        println!("ls after the first sweep: {listed:?}");
        assert!(listed.contains(&vm), "{vm} is gone before its life ended");
        assert_eq!(vm_of(&td, "live").as_deref(), Some(vm.as_str()));
        // At its `expires_at`, the sweep timer removes it.
        let past_life = Duration::from_secs(life + 20);
        tokio::time::sleep(past_life.saturating_sub(up_at.elapsed())).await;
        let listed = names(&api.ls().await.unwrap());
        println!("ls after its life: {listed:?}");
        assert!(!listed.contains(&vm), "{vm} outlived its life");
        assert!(vm_of(&td, "live").is_none());
        assert_eq!(
            destroyed(&td, "live"),
            vec![(vm.clone(), "exedev_expired".to_owned())]
        );
        assert_eq!(td.get("/api/targets/live").await.ok()["state"], "absent");
        println!("sweep after: {:?}", sweep(&td).await);
        vm
    })
    .await;
    // Remove every VM of this run that is left, then report.
    let left: Vec<String> = names(&api.ls().await.unwrap())
        .into_iter()
        .filter(|n| n.starts_with(prefix))
        .collect();
    if !left.is_empty() {
        api.rm(&left).await.unwrap();
    }
    let after = names(&api.ls().await.unwrap());
    println!("ls after: {after:?}");
    let vm = body.unwrap();
    assert!(left.is_empty(), "the sweep left {left:?}");
    assert!(after.iter().all(|n| n != &vm && !n.starts_with(prefix)));
}
