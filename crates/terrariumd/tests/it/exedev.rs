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
//! - TER-653: two daemons on one exe.dev account keep each other's VMs (an
//!   instance id in the name prefix). The sweep reads the holders again
//!   under the target lock. A target whose file goes, and a target whose
//!   `api_token` changes, keep the VM on the account it was made on. A
//!   record with no ssh host stays through a miss in `ls` until its grace
//!   time.
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
/// A second token of the same fake account.
const TOKEN2: &str = "exe1.fixture-s3cret-token-2";

/// The fake account: its VMs (name to tags and creation time), each
/// command it got, and the faults to inject.
#[derive(Default)]
struct Fake {
    vms: BTreeMap<String, (Vec<String>, chrono::DateTime<chrono::Utc>)>,
    commands: Vec<String>,
    /// The token of each command, in the order of `commands`.
    tokens: Vec<String>,
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
                let auth = h
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "))
                    .unwrap_or("")
                    .to_owned();
                if auth != TOKEN && auth != TOKEN2 {
                    return (StatusCode::UNAUTHORIZED, "bad token".to_owned());
                }
                let words: Vec<String> = body.split_whitespace().map(str::to_owned).collect();
                let delay = {
                    let mut g = f.lock().unwrap();
                    g.commands.push(body.clone());
                    g.tokens.push(auth);
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
/// While the file `ssh-slow` is in `dir`, it waits before it runs.
pub(crate) fn fake_ssh(dir: &std::path::Path) -> (String, std::path::PathBuf) {
    let log = dir.join("ssh.log");
    let bin = dir.join("fake-ssh");
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\nwhile [ -e '{}' ]; do sleep 0.1; done\nfor a; do last=\"$a\"; done\nprev=\"\"\nfor a; do [ \"$prev\" = \"--\" ] && echo \"$a\" >> '{}'; prev=\"$a\"; done\nexec sh -c \"$last\"\n",
            dir.join("ssh-slow").display(),
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

pub(crate) async fn output(td: &TestDaemon, target: &str, sid: &str) -> (String, Value) {
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

#[tokio::test]
async fn durable_exedev_finishes_offline_once_with_real_exit() {
    let (mut td, _fake, _, url, ssh) = daemon().await;
    let root = td.dir.path().join("remote-spool");
    let _cleanup = super::ssh_restart::RemoteCleanup(root.clone());
    let mut def = target("restart-ex", 8 * 3600);
    def["work_root"] = json!(root);
    td.post("/api/targets", def).await.ok();
    td.post_empty("/api/targets/restart-ex/up").await.ok();
    let (aid, wid) = super::ssh_restart::start(&td, "restart-ex", root.to_str().unwrap()).await;
    super::ssh_restart::started(&td, &wid, &root).await;
    td.daemon.take().unwrap().shutdown_and_wait().await;
    std::fs::write(root.join("go"), "go").unwrap();
    let spool = std::fs::read_dir(root.join(".terrarium-runs"))
        .unwrap()
        .find(|entry| entry.as_ref().unwrap().file_type().unwrap().is_dir())
        .unwrap()
        .unwrap()
        .path();
    super::ssh_restart::wait_file(&spool.join("exit")).await;
    assert_eq!(std::fs::read_to_string(spool.join("exit")).unwrap(), "23\n");
    reboot(&mut td, &url, &ssh).await;
    super::ssh_restart::completed(&td, &aid, &wid, &root).await;
    reboot(&mut td, &url, &ssh).await;
    super::ssh_restart::completed(&td, &aid, &wid, &root).await;
}

#[tokio::test]
async fn durable_expired_checkpoint_does_not_relaunch_or_prevent_boot() {
    let (mut td, _fake, _, url, ssh) = daemon().await;
    let root = td.dir.path().join("expired-spool");
    let _cleanup = super::ssh_restart::RemoteCleanup(root.clone());
    let mut def = target("expired-ex", 8 * 3600);
    def["work_root"] = json!(root);
    td.post("/api/targets", def).await.ok();
    td.post_empty("/api/targets/expired-ex/up").await.ok();
    let command = format!(
        "echo once >> '{}'; echo retained; exit 17",
        root.join("launches").display()
    );
    let opened = td
        .post(
            "/api/targets/expired-ex/exec",
            json!({"cmd":["sh","-c",command],"durable":true}),
        )
        .await;
    let sid = opened.ok()["session_id"].as_str().unwrap().to_owned();
    let (stdout, end) = output(&td, "expired-ex", &sid).await;
    assert_eq!(stdout, "retained\n");
    assert_eq!(end["code"], 17);
    let spool = std::fs::read_dir(root.join(".terrarium-runs"))
        .unwrap()
        .find(|entry| entry.as_ref().unwrap().file_type().unwrap().is_dir())
        .unwrap()
        .unwrap()
        .path();
    // Simulate remote output GC while retaining its claim tombstone.
    std::fs::write(spool.join("expired"), "").unwrap();
    reboot(&mut td, &url, &ssh).await;
    let (_, end) = output(&td, "expired-ex", &sid).await;
    assert_eq!(end["code"], "durable_expired");
    let journal = terrariumd::hosts::resume::Journal::new(&td.state);
    let mut checkpoints = journal.scan::<Value>("ssh/").unwrap();
    assert_eq!(checkpoints.len(), 1);
    let (key, checkpoint) = checkpoints.first_mut().unwrap();
    checkpoint["created"] = json!((chrono::Utc::now() - chrono::Duration::days(9)).timestamp());
    journal.save(key, checkpoint).unwrap();
    reboot(&mut td, &url, &ssh).await;
    let (_, end) = output(&td, "expired-ex", &sid).await;
    assert_eq!(end["type"], "error");
    assert_eq!(end["code"], "interrupted");
    assert_eq!(
        std::fs::read_to_string(root.join("launches")).unwrap(),
        "once\n"
    );
}

#[tokio::test]
async fn durable_exedev_recreated_vm_is_not_contacted() {
    let (mut td, _fake, ssh_log, url, ssh) = daemon().await;
    let root = td.dir.path().join("replacement-spool");
    let _cleanup = super::ssh_restart::RemoteCleanup(root.clone());
    let mut def = target("replacement-ex", 8 * 3600);
    def["work_root"] = json!(root);
    td.post("/api/targets", def).await.ok();
    td.post_empty("/api/targets/replacement-ex/up").await.ok();
    let opened = td
        .post(
            "/api/targets/replacement-ex/exec",
            json!({"durable":true,"cmd":["sh","-c","echo retained; exit 19"]}),
        )
        .await;
    let sid = opened.ok()["session_id"].as_str().unwrap().to_owned();
    assert_eq!(output(&td, "replacement-ex", &sid).await.1["code"], 19);
    td.daemon.take().unwrap().shutdown_and_wait().await;
    let before = std::fs::read_to_string(&ssh_log).unwrap();
    // Same target definition and SSH destination, but a different VM incarnation.
    db(&td)
        .execute(
            "UPDATE exedev_vms SET name=name || '-replacement' WHERE target='replacement-ex'",
            [],
        )
        .unwrap();
    reboot(&mut td, &url, &ssh).await;
    let (_, end) = output(&td, "replacement-ex", &sid).await;
    assert_eq!(end["code"], "interrupted", "{end}");
    assert!(
        end["message"].as_str().unwrap().contains("target changed"),
        "{end}"
    );
    assert_eq!(std::fs::read_to_string(&ssh_log).unwrap(), before);
}

#[tokio::test]
async fn durable_startup_failures_end_promptly_with_reason() {
    let (td, _fake, _, _, _) = daemon().await;
    let tools = td.dir.path().join("limited-tools");
    std::fs::create_dir(&tools).unwrap();
    // Keep every prerequisite except mkfifo. No remote command may start.
    for tool in [
        "setsid", "nohup", "flock", "sh", "timeout", "date", "head", "tail", "base64", "tr", "wc",
        "cat", "mkdir", "mv", "rm", "rmdir", "sleep",
    ] {
        std::os::unix::fs::symlink(format!("/usr/bin/{tool}"), tools.join(tool)).unwrap();
    }
    for (name, env, reason) in [
        (
            "missing-tool-ex",
            json!({"PATH":tools}),
            "unsupported: mkfifo missing",
        ),
        ("bad-spool-ex", json!({}), "spool error"),
    ] {
        let root = td.dir.path().join(name);
        let mut def = target(name, 8 * 3600);
        def["work_root"] = json!(root);
        td.post("/api/targets", def).await.ok();
        td.post_empty(&format!("/api/targets/{name}/up")).await.ok();
        if name == "bad-spool-ex" {
            std::fs::write(&root, "not a directory").unwrap();
        }
        let began = tokio::time::Instant::now();
        let opened = td
            .post(
                &format!("/api/targets/{name}/exec"),
                json!({"durable":true,"env":env,"cmd":["true"]}),
            )
            .await;
        let sid = opened.ok()["session_id"].as_str().unwrap().to_owned();
        let (_, end) = output(&td, name, &sid).await;
        assert_eq!(end["code"], "unsupported", "{end}");
        assert!(end["message"].as_str().unwrap().contains(reason), "{end}");
        assert!(began.elapsed() < Duration::from_secs(5));
    }
    std::os::unix::fs::symlink("/usr/bin/mkfifo", tools.join("mkfifo")).unwrap();
    let root = td.dir.path().join("missing-python");
    let mut def = target("missing-python-ex", 8 * 3600);
    def["work_root"] = json!(root);
    td.post("/api/targets", def).await.ok();
    td.post_empty("/api/targets/missing-python-ex/up")
        .await
        .ok();
    let opened = td
        .post(
            "/api/targets/missing-python-ex/exec",
            json!({"durable":true,"env":{"PATH":tools},"cmd":["true"]}),
        )
        .await;
    let sid = opened.ok()["session_id"].as_str().unwrap().to_owned();
    let (_, end) = output(&td, "missing-python-ex", &sid).await;
    assert_eq!(end["code"], "unsupported", "{end}");
    assert!(
        end["message"].as_str().unwrap().contains("python3 missing"),
        "{end}"
    );
}

fn db(td: &TestDaemon) -> rusqlite::Connection {
    terrariumd::db::open(&td.state.join("terrarium.db")).unwrap()
}

/// The name prefix of the VMs of this daemon (TER-653).
fn prefix(td: &TestDaemon) -> String {
    terrariumd::exedev::prefix_of(&terrariumd::exedev::instance(&db(td)).unwrap())
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

/// Write a record of a VM of `target`, `age_secs` old, with an 8 hour life.
fn insert_record(td: &TestDaemon, name: &str, target: &str, ssh_dest: Option<&str>, age_secs: i64) {
    let at = chrono::Utc::now() - chrono::Duration::seconds(age_secs);
    db(td)
        .execute(
            "INSERT INTO exedev_vms (name, target, project, api_token, admin_set, tag,
                                     ssh_dest, created_at, expires_at)
             VALUES (?1, ?2, NULL, 'exedev-api-token', 1, 'terrarium-dev-ssh', ?3, ?4, ?5)",
            rusqlite::params![
                name,
                target,
                ssh_dest,
                at.to_rfc3339(),
                (at + chrono::Duration::hours(8)).to_rfc3339()
            ],
        )
        .unwrap();
}

/// Whether a VM of this name has a record.
fn recorded(td: &TestDaemon, name: &str) -> bool {
    db(td)
        .query_row(
            "SELECT count(*) FROM exedev_vms WHERE name = ?1",
            [name],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
        == 1
}

/// The number of `ls` calls that the fake got.
fn ls_calls(fake: &FakeRef) -> usize {
    fake.lock()
        .unwrap()
        .commands
        .iter()
        .filter(|c| c.starts_with("ls "))
        .count()
}

/// The recorded VM of a target, and its ssh host.
fn record(td: &TestDaemon, target: &str) -> Option<(String, Option<String>)> {
    use rusqlite::OptionalExtension;
    db(td)
        .query_row(
            "SELECT name, ssh_dest FROM exedev_vms WHERE target = ?1
             ORDER BY created_at DESC",
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

#[tokio::test]
async fn workspace_sync_uses_exedev_work_root_and_pulls_output() {
    workspace_sync_roundtrip(true).await;
}

#[tokio::test]
async fn workspace_sync_uses_exedev_home_without_work_root() {
    workspace_sync_roundtrip(false).await;
}

async fn workspace_sync_roundtrip(work_root: bool) {
    // These round trips use real rsync on both sides of the fake SSH transport.
    // Keep the production security floor, but allow hosts with older rsync to
    // run the rest of the exe.dev tests with their default PATH.
    let version_output = tokio::process::Command::new("rsync")
        .arg("--version")
        .output()
        .await;
    let supported = version_output.is_ok_and(|output| {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let version = stdout
            .strip_prefix("rsync  version ")
            .and_then(|s| s.split_whitespace().next())
            .map(|s| s.split('.').map(str::parse::<u32>).collect::<Vec<_>>());
        output.status.success()
            && matches!(version.as_deref(), Some([Ok(major), Ok(minor), Ok(patch)]) if (*major, *minor, *patch) >= (3, 4, 0))
    });
    if !supported {
        eprintln!("SKIP: exe.dev workspace sync round trip requires rsync >= 3.4.0 on PATH");
        return;
    }
    let (td, _, _, _, ssh) = daemon().await;
    let base = td.dir.path().join("remote-work");
    // ssh_plan sends one shell command, whereas rsync sends a server argv.
    std::fs::write(&ssh, format!("#!/bin/sh\nexport HOME='{}'\nwhile [ \"$#\" -gt 0 ]; do case \"$1\" in *.exe.xyz) shift; break;; esac; shift; done\nif [ \"$#\" = 1 ]; then exec sh -c \"$1\"; else exec \"$@\"; fi\n", base.display())).unwrap();
    let mut definition = target("sync", 3600);
    definition["sync_workspace"] = json!(true);
    if work_root {
        definition["work_root"] = json!(base);
    }
    td.post("/api/targets", definition).await.ok();
    td.post_empty("/api/targets/sync/up").await.ok();
    td.project("p").await;
    let agent = td.agent("p", "worker", json!({})).await;
    let id = agent["id"].as_str().unwrap();
    let local = state(&td).work_dir.join("p/worker");
    std::fs::create_dir_all(local.join("repo")).unwrap();
    std::fs::write(local.join("repo/input"), "source").unwrap();
    let response = td
        .post(
            "/api/targets/sync/exec",
            json!({
                "agent": id,
                "cwd": local.join("repo"),
                "cmd": ["sh", "-c", format!("cat input > {}/repo/output; pwd", local.display())]
            }),
        )
        .await;
    let session = response.ok()["session_id"].as_str().unwrap();
    let (stdout, end) = output(&td, "sync", session).await;
    assert_eq!(end["code"], 0, "{end}");
    assert_eq!(
        stdout.trim(),
        base.join("p/worker/workspace/repo").to_str().unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(local.join("repo/output")).unwrap(),
        "source"
    );
    if work_root {
        // Turn the holder into a bound card worker. Its first exec must not
        // adopt the directory made by the earlier non-card sync.
        let owned = base.join("p/worker");
        db(&td)
            .execute(
                "INSERT INTO spawns (id, owner, owner_name, project, name, template, harness, task,
             spawned_by, spawned_at, deadline_at, bound_kind, bound_id)
             VALUES (?1, 'operator', 'operator', 'p', 'worker', 'worker', 'claude', 'test',
             'operator', '2026-01-01', '2099-01-01', 'card', 'TER-918')",
                [id],
            )
            .unwrap();
        let actor = terrariumd::actor::Actor::Agent(terrariumd::actor::AgentActor {
            id: id.into(),
            name: "worker".into(),
            project: "p".into(),
            roles: vec![terrarium_core::Role::Sysadmin],
            policy: terrarium_core::config::Policy {
                targets: vec!["sync".into()],
                ..Default::default()
            },
        });
        let command = || terrariumd::executor::RemoteCmd {
            argv: vec!["sh".into(), "-c".into(), "cat input; pwd".into()],
            cwd: Some(local.join("repo").display().to_string()),
            ..Default::default()
        };
        let error = terrariumd::executor::start_exec(&state(&td), &actor, "sync", command())
            .await
            .err()
            .unwrap();
        assert!(
            error.message.contains("cannot prepare owned target root"),
            "{error:?}"
        );
        assert!(!owned.join(".terrarium-owner").exists());
        std::fs::remove_dir_all(&owned).unwrap();
        for run in [false, false, true] {
            let session = if run {
                terrariumd::executor::start_run_exec(&state(&td), &actor, "sync", command(), vec![])
                    .await
            } else {
                terrariumd::executor::start_exec(&state(&td), &actor, "sync", command()).await
            }
            .unwrap();
            let (stdout, end) = output(&td, "sync", &session.id).await;
            assert_eq!(end["code"], 0, "{end}: {stdout}");
            assert!(stdout.contains("source"), "{stdout}");
            assert!(owned.join(".terrarium-owner").is_file());
            assert!(owned.join("workspace/repo/input").is_file());
        }
        db(&td)
            .execute("DELETE FROM agents WHERE id=?1", [id])
            .unwrap();
        terrariumd::target_roots::sweep(&state(&td)).await.unwrap();
        assert!(!owned.exists());
    }
}

#[tokio::test]
async fn workspace_sync_reports_mirror_creation_failure() {
    let (td, _, _, _, ssh) = daemon().await;
    let mut definition = target("sync", 3600);
    definition["sync_workspace"] = json!(true);
    definition["work_root"] = json!("/home/exedev/work");
    td.post("/api/targets", definition).await.ok();
    td.patch("/api/targets/sync", json!({"sync_workspace":true}))
        .await
        .ok();
    td.post_empty("/api/targets/sync/up").await.ok();
    td.project("p").await;
    let agent = td.agent("p", "worker", json!({})).await;
    let agent = agent["id"].as_str().unwrap();
    // Simulate the remote user's refusal to create the absolute sandbox path.
    std::fs::write(&ssh, "#!/bin/sh\nexit 1\n").unwrap();
    let response = td
        .post(
            "/api/targets/sync/exec",
            json!({"agent":agent,"cmd":["true"]}),
        )
        .await;
    response.err(422, "invalid");
    assert!(
        response
            .body
            .to_string()
            .contains("could not create remote workspace"),
        "{}",
        response.body
    );
    assert_eq!(
        td.get("/api/targets/sync").await.ok()["sync_workspace"],
        true
    );
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
    let p = prefix(&td);
    let vm1 = vm_of(&td, "ex1").unwrap();
    assert!(vm1.starts_with(&format!("{p}ex1-")), "{vm1}");
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
        f.add(&format!("{p}lost-1"), "terrarium-dev-ssh", 3600);
        f.add(&format!("{p}young-1"), "terrarium-dev-ssh", 0);
        f.add("personal", "terrarium-dev-ssh", 3600);
        f.add(&format!("{p}other-1"), "other", 3600);
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
            (format!("{p}lost-1"), "leftover".to_owned()),
        ]
    );
    assert_eq!(
        fake.lock().unwrap().names(),
        vec![
            "personal".to_owned(),
            format!("{p}other-1"),
            format!("{p}young-1")
        ]
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

/// TER-651: `target test` on an exedev target logs in to its VM through
/// the ssh target of the VM. With no VM, or a VM with no ssh host yet, it
/// is `409` with a message that says to bring the VM up.
#[tokio::test]
async fn target_test_on_an_exedev_target_logs_in_to_its_vm() {
    let (td, _fake, ssh_log, _, _) = daemon().await;
    td.post("/api/targets", target("ex", 8 * 3600)).await.ok();

    let r = td.post_empty("/api/targets/ex/test").await;
    assert_eq!(r.status.as_u16(), 409, "{}", r.body);
    let msg = r.body["error"]["message"].as_str().unwrap();
    assert!(msg.contains("has no exe.dev VM"), "{msg}");
    assert!(msg.contains("terrarium admin target verb ex up"), "{msg}");

    insert_record(&td, "vm-nohost", "ex", None, 0);
    let r = td.post_empty("/api/targets/ex/test").await;
    assert_eq!(r.status.as_u16(), 409, "{}", r.body);
    let msg = r.body["error"]["message"].as_str().unwrap();
    assert!(msg.contains("has no ssh host yet"), "{msg}");
    db(&td)
        .execute("DELETE FROM exedev_vms WHERE target = 'ex'", [])
        .unwrap();

    td.post_empty("/api/targets/ex/up").await.ok();
    let vm = vm_of(&td, "ex").unwrap();
    let r = td.post_empty("/api/targets/ex/test").await;
    let b = r.ok().clone();
    assert_eq!(b["result"], "ok", "{b}");
    assert_eq!(b["target"], "ex", "{b}");
    assert_eq!(b["kind"], "exedev", "{b}");
    let addr = format!("exedev@{vm}.exe.xyz");
    assert_eq!(b["address"], addr.as_str(), "{b}");
    assert_eq!(b["credential"], "vmkey", "{b}");
    let log = std::fs::read_to_string(&ssh_log).unwrap();
    assert!(log.contains(&addr), "{log}");
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

/// TER-653 finding 1: two daemons on one exe.dev account (the main daemon
/// and a test, staging or dev daemon) each have their own instance id in
/// the name prefix. The sweep of each removes only its own leftovers and
/// keeps the other daemon's VMs, also when they are old in `ls`.
#[tokio::test]
async fn two_daemons_on_one_exe_dev_account_keep_each_others_vms() {
    let (a, fake, _, url, _) = daemon().await;
    let mut b = TestDaemon::start().await;
    let (ssh_b, _) = fake_ssh(b.dir.path());
    reboot(&mut b, &url, &ssh_b).await;
    secrets(&b, TOKEN, None).await;
    let (pa, pb) = (prefix(&a), prefix(&b));
    assert_ne!(pa, pb);
    for td in [&a, &b] {
        td.post("/api/targets", target("ex", 8 * 3600)).await.ok();
        let r = td.post_empty("/api/targets/ex/up").await;
        assert_eq!(r.ok()["state"], "running", "{}", r.body);
    }
    let (va, vb) = (vm_of(&a, "ex").unwrap(), vm_of(&b, "ex").unwrap());
    assert!(va.starts_with(&format!("{pa}ex-")), "{va}");
    assert!(vb.starts_with(&format!("{pb}ex-")), "{vb}");
    {
        let mut f = fake.lock().unwrap();
        f.add(&format!("{pa}lost-1"), "terrarium-dev-ssh", 0);
        f.add(&format!("{pb}lost-1"), "terrarium-dev-ssh", 0);
        // `ls` shows each VM as an hour old: past the leftover grace time.
        f.ls_older_secs = 3600;
    }
    assert_eq!(
        sweep(&a).await,
        vec![(format!("{pa}lost-1"), "leftover".to_owned())]
    );
    assert_eq!(
        sweep(&b).await,
        vec![(format!("{pb}lost-1"), "leftover".to_owned())]
    );
    let mut want = vec![va.clone(), vb.clone()];
    want.sort();
    assert_eq!(fake.lock().unwrap().names(), want);
    assert_eq!(vm_of(&a, "ex"), Some(va));
    assert_eq!(vm_of(&b, "ex"), Some(vb));
}

/// TER-653 finding 2: the sweep plans an unheld VM, then waits for the
/// target lock (a slow `up` holds it). An agent that holds the target
/// starts in that time. Under the lock the sweep reads the holders again
/// and keeps the VM.
#[tokio::test]
async fn the_sweep_reads_the_holders_again_under_the_target_lock() {
    let (td, fake, _, _, _) = daemon().await;
    td.post("/api/targets", target("ex", 8 * 3600)).await.ok();
    td.post_empty("/api/targets/ex/up").await.ok();
    let vm = vm_of(&td, "ex").unwrap();
    age(&td, "ex", 3600);
    // An agent that holds the target by its policy; not running yet.
    td.project("p").await;
    let a = td
        .agent("p", "w", json!({"policy": {"targets": ["ex"]}}))
        .await;
    let w = a["id"].as_str().unwrap().to_owned();
    // The adopting `up` holds the target lock while its ssh check waits.
    let slow = td.dir.path().join("ssh-slow");
    std::fs::write(&slow, "").unwrap();
    let before = ls_calls(&fake);
    let up = td.post_empty("/api/targets/ex/up");
    let swept = async {
        // After the `ls` of the `up`: the `up` holds the lock.
        while ls_calls(&fake) < before + 1 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        td.post_empty("/api/exedev/sweep").await
    };
    let holder = async {
        // After the `ls` of the sweep, it reads the holders (none) and
        // waits for the lock.
        while ls_calls(&fake) < before + 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        td.post_empty(&format!("/api/agents/{w}/start")).await.ok();
        td.wait_status(&w, "idle").await;
        std::fs::remove_file(&slow).unwrap();
    };
    let (up, swept, ()) = tokio::join!(up, swept, holder);
    assert_eq!(up.ok()["state"], "running", "{}", up.body);
    let body = swept.ok().clone();
    assert_eq!(body["removed"], json!([]), "{body}");
    assert_eq!(body["errors"], json!([]), "{body}");
    assert_eq!(fake.lock().unwrap().names(), vec![vm.clone()]);
    assert_eq!(vm_of(&td, "ex"), Some(vm));
}

/// TER-653 finding 3: a target from a file whose file goes is deleted on
/// reload. Its VM's record stays, and the sweep removes the VM as
/// `orphaned`.
#[tokio::test]
async fn a_vm_of_a_target_whose_file_goes_is_removed_by_the_sweep() {
    let (td, fake, _, _, _) = daemon().await;
    crate::common::write(
        &td.config,
        "targets/fx.toml",
        "kind = \"exedev\"\nephemeral = true\ncredential = \"vmkey\"\n",
    );
    td.post_empty("/api/reload").await.ok();
    assert_eq!(td.get("/api/targets/fx").await.ok()["source"], "file");
    let r = td.post_empty("/api/targets/fx/up").await;
    assert_eq!(r.ok()["state"], "running", "{}", r.body);
    let vm = vm_of(&td, "fx").unwrap();
    std::fs::remove_file(td.config.join("targets/fx.toml")).unwrap();
    td.post_empty("/api/reload").await.ok();
    td.get("/api/targets/fx").await.err(404, "not_found");
    assert_eq!(vm_of(&td, "fx").as_deref(), Some(vm.as_str()));
    assert_eq!(sweep(&td).await, vec![(vm, "orphaned".to_owned())]);
    assert!(fake.lock().unwrap().vms.is_empty());
    assert!(vm_of(&td, "fx").is_none());
}

/// TER-653 finding 3: the `[exedev] api_token` of a target file changes
/// while the target has a VM (an API `PATCH` refuses it on a running
/// target; a file reload does not). `down` removes the VM with the token
/// it was made with; the next `up` uses the new token.
#[tokio::test]
async fn a_changed_api_token_removes_the_vm_on_the_account_it_was_made_on() {
    let (td, fake, _, _, _) = daemon().await;
    let r = td
        .post(
            "/api/secrets",
            json!({"name": "exedev-other", "kind": "api_key", "fields": {"value": TOKEN2}}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 201, "{}", r.body);
    let file = "kind = \"exedev\"\nephemeral = true\ncredential = \"vmkey\"\n";
    crate::common::write(&td.config, "targets/ex.toml", file);
    td.post_empty("/api/reload").await.ok();
    td.post_empty("/api/targets/ex/up").await.ok();
    let vm = vm_of(&td, "ex").unwrap();
    crate::common::write(
        &td.config,
        "targets/ex.toml",
        &format!("{file}[exedev]\napi_token = \"exedev-other\"\n"),
    );
    td.post_empty("/api/reload").await.ok();
    let def = td.get("/api/targets/ex").await.ok().clone();
    assert_eq!(def["exedev"]["api_token"], "exedev-other", "{def}");
    // Two accounts now (the record's and the target's): the young VM stays.
    assert!(sweep(&td).await.is_empty());
    assert_eq!(fake.lock().unwrap().names(), vec![vm.clone()]);
    let from = fake.lock().unwrap().commands.len();
    let r = td.post_empty("/api/targets/ex/down").await;
    assert_eq!(r.ok()["state"], "absent", "{}", r.body);
    td.post_empty("/api/targets/ex/up").await.ok();
    let f = fake.lock().unwrap();
    let used: Vec<(&str, &str)> = f.commands[from..]
        .iter()
        .zip(&f.tokens[from..])
        .filter_map(|(c, t)| Some((c.split_whitespace().next()?, t.as_str())))
        .filter(|(c, _)| *c == "rm" || *c == "new")
        .collect();
    assert_eq!(
        used,
        vec![("rm", TOKEN), ("new", TOKEN2)],
        "{:?}",
        f.commands
    );
    assert!(!f.vms.contains_key(&vm));
    assert_eq!(f.vms.len(), 1);
}

/// TER-653 finding 4: a record with no ssh host (its `new` may still run)
/// that `ls` does not list stays through `status` and `up`. The `up` makes
/// a new VM. When the first VM shows up late, the sweep removes it as
/// `unready` after the grace time.
#[tokio::test]
async fn a_record_with_no_ssh_host_stays_through_an_ls_miss_until_its_grace_time() {
    let (td, fake, _, _, _) = daemon().await;
    td.post("/api/targets", target("ex", 8 * 3600)).await.ok();
    let late = "terrarium-x-ex-late";
    insert_record(&td, late, "ex", None, 0);
    let r = td.post_empty("/api/targets/ex/status").await;
    assert_eq!(r.ok()["state"], "absent", "{}", r.body);
    assert!(recorded(&td, late), "status kept the record");
    let r = td.post_empty("/api/targets/ex/up").await;
    assert_eq!(r.ok()["state"], "running", "{}", r.body);
    assert!(recorded(&td, late), "up kept the record");
    let new = vm_of(&td, "ex").unwrap();
    assert_ne!(new, late);
    // The first `new` lands late; after the grace time the sweep removes it.
    fake.lock().unwrap().add(late, "terrarium-dev-ssh", 0);
    assert!(sweep(&td).await.is_empty());
    let back = (chrono::Utc::now() - chrono::Duration::seconds(3600)).to_rfc3339();
    db(&td)
        .execute(
            "UPDATE exedev_vms SET created_at = ?2 WHERE name = ?1",
            rusqlite::params![late, back],
        )
        .unwrap();
    assert_eq!(
        sweep(&td).await,
        vec![(late.to_owned(), "unready".to_owned())]
    );
    assert_eq!(fake.lock().unwrap().names(), vec![new.clone()]);
    assert!(!recorded(&td, late));
    // Past the grace time, a miss in `ls` forgets a record with no ssh host.
    let old = "terrarium-x-ex-old";
    td.post_empty("/api/targets/ex/down").await.ok();
    insert_record(&td, old, "ex", None, 3600);
    let r = td.post_empty("/api/targets/ex/status").await;
    assert_eq!(r.ok()["state"], "absent", "{}", r.body);
    assert!(!recorded(&td, old));
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
    // Its own instance id (TER-653): its sweep never takes a VM of another
    // daemon on the account, and the clean-up below takes only its own.
    let inst = format!(
        "live{:04x}",
        chrono::Utc::now().timestamp_subsec_micros() & 0xffff
    );
    let prefix = terrariumd::exedev::prefix_of(&inst);
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
        let conn = db(&td);
        conn.execute("DELETE FROM exedev_instance", []).unwrap();
        conn.execute("INSERT INTO exedev_instance (id) VALUES (?1)", [&inst])
            .unwrap();
        secrets(&td, &token, Some(&key)).await;
        td.post("/api/targets", target("live", life)).await.ok();
        let r = td.post_empty("/api/targets/live/up").await;
        assert_eq!(r.ok()["state"], "running", "{}", r.body);
        let up_at = std::time::Instant::now();
        let vm = vm_of(&td, "live").unwrap();
        assert!(
            vm.starts_with(&terrariumd::exedev::prefix_of(&inst)),
            "{vm}"
        );
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
        .filter(|n| n.starts_with(&prefix))
        .collect();
    if !left.is_empty() {
        api.rm(&left).await.unwrap();
    }
    let after = names(&api.ls().await.unwrap());
    println!("ls after: {after:?}");
    let vm = body.unwrap();
    assert!(left.is_empty(), "the sweep left {left:?}");
    assert!(after.iter().all(|n| n != &vm && !n.starts_with(&prefix)));
}
