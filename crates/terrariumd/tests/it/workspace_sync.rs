//! API coverage using a private sshd with a chroot filesystem. The remote
//! workspace has the same absolute path without sharing the daemon's files.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common::TestDaemon;
use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn copy_binary(jail: &Path, binary: &str) {
    let copy = |source: &Path| {
        let dest = jail.join(source.strip_prefix("/").unwrap());
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::copy(source, dest).unwrap();
    };
    copy(Path::new(binary));
    let libs = std::process::Command::new("ldd").arg(binary).output().unwrap();
    assert!(libs.status.success());
    for word in String::from_utf8(libs.stdout).unwrap().split_whitespace() {
        if word.starts_with('/') { copy(Path::new(word)); }
    }
}

struct Ssh {
    _dir: tempfile::TempDir,
    jail: PathBuf,
    key: PathBuf,
    port: u16,
    server: tokio::process::Child,
}

impl Ssh {
    async fn start(sshd: std::ffi::OsString) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let jail = dir.path().join("jail");
        std::fs::create_dir(&jail).unwrap();
        std::fs::set_permissions(&jail, std::fs::Permissions::from_mode(0o755)).unwrap();
        for binary in ["/bin/sh", "/bin/bash", "/usr/bin/rsync", "/bin/cat", "/bin/mkdir", "/bin/mv", "/bin/rm", "/usr/bin/wc", "/bin/chmod", "/usr/bin/dirname", "/bin/sleep", "/usr/bin/base64"] {
            copy_binary(&jail, binary);
        }
        for name in ["tmp", "root", "dev", "etc"] { std::fs::create_dir(jail.join(name)).unwrap(); }
        std::fs::write(jail.join("dev/null"), "").unwrap();
        let key = dir.path().join("key");
        assert!(std::process::Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&key).status().unwrap().success());
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let config = dir.path().join("config");
        std::fs::write(&config, format!("Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nAuthorizedKeysFile {}.pub\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nPermitRootLogin yes\nChrootDirectory {}\nPidFile {}/pid\n", key.display(), key.display(), jail.display(), dir.path().display())).unwrap();
        let mut server = tokio::process::Command::new(sshd).args(["-D", "-e", "-f"]).arg(config).kill_on_drop(true).spawn().unwrap();
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() { break; }
            assert!(server.try_wait().unwrap().is_none(), "private sshd exited");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Self { _dir: dir, jail, key, port, server }
    }
}

async fn done(td: &TestDaemon, sid: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut since = 0;
        loop {
            let r = td.get(&format!("/api/targets/synced/sessions/{sid}/output?since={since}&wait=1")).await;
            let body = r.ok();
            since = body["next"].as_u64().unwrap();
            if body["done"] == true { return body["end"].clone(); }
        }
    }).await.unwrap()
}

#[tokio::test]
async fn ssh_api_exec_run_copy_and_conflict() {
    // Explicit opt-in: the fixture needs root for chroot, openssh-server,
    // openssh-client, rsync, and the standard dynamically linked Linux tools.
    let Some(sshd) = std::env::var_os("TERRARIUM_TEST_SSHD") else { return };
    let mut ssh = Ssh::start(sshd).await;
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.post("/api/secrets", json!({"name":"sync-key", "kind":"ssh_key", "fields":{
        "private_key":std::fs::read_to_string(&ssh.key).unwrap()
    }})).await.ok();
    td.post("/api/targets", json!({"name":"synced", "kind":"ssh", "address":"root@127.0.0.1", "port":ssh.port, "credential":"sync-key", "sync_workspace":true})).await.ok();
    let agent = td.agent("p", "worker", json!({"policy":{"targets":["synced"]}})).await;
    let agent = agent["id"].as_str().unwrap();
    let root = td.state.with_file_name("work").join("p/worker");
    std::fs::create_dir_all(&root).unwrap();
    let remote = ssh.jail.join(root.strip_prefix("/").unwrap());
    std::fs::write(root.join("input"), "pushed").unwrap();
    std::fs::write(root.join(".env"), "never ship").unwrap();
    let started = td.post("/api/targets/synced/exec", json!({"agent":agent, "cwd":root, "cmd":["sh","-c","cat input > output; sleep 1"]})).await;
    let sid = started.ok()["session_id"].as_str().unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !remote.join("output").exists() { tokio::time::sleep(Duration::from_millis(10)).await; }
    }).await.unwrap();
    assert!(!root.join("output").exists());
    assert_eq!(td.get(&format!("/api/targets/synced/sessions/{sid}")).await.ok()["done"], false);
    assert_eq!(done(&td, sid).await["code"], 0);
    assert_eq!(std::fs::read_to_string(root.join("output")).unwrap(), "pushed");
    assert!(!remote.join(".env").exists());

    let run = td.post_as(agent, &format!("/api/agents/{agent}/run"), json!({"target":"synced", "cwd":root, "cmd":["sh","-c","cat output > run-output"]})).await;
    let sid = run.ok()["session_id"].as_str().unwrap();
    assert_eq!(done(&td, sid).await["code"], 0);
    assert_eq!(std::fs::read_to_string(root.join("run-output")).unwrap(), "pushed");

    let cp = td.post("/api/targets/synced/cp", json!({"agent":agent,"direction":"push","path":root.join("copied")})).await;
    let sid = cp.ok()["session_id"].as_str().unwrap();
    let bytes = b"copied data";
    td.post(&format!("/api/targets/synced/sessions/{sid}/input"), json!({"data":base64::engine::general_purpose::STANDARD.encode(bytes),"finish":{"bytes":bytes.len(),"sha256":terrarium_host::protocol::hex(&Sha256::digest(bytes))}})).await.ok();
    assert_eq!(done(&td, sid).await["type"], "cp");
    assert_eq!(std::fs::read(root.join("copied")).unwrap(), bytes);

    let cp = td.post("/api/targets/synced/cp", json!({"agent":agent,"direction":"pull","path":root.join("copied")})).await;
    let sid = cp.ok()["session_id"].as_str().unwrap();
    assert_eq!(done(&td, sid).await["type"], "cp");

    let started = td.post("/api/targets/synced/exec", json!({"agent":agent,"cwd":root,"cmd":["sh","-c","echo remote > conflict; sleep 1"]})).await;
    let sid = started.ok()["session_id"].as_str().unwrap();
    std::fs::write(root.join("conflict"), "harness").unwrap();
    let end = done(&td, sid).await;
    assert_eq!(end["code"], "sync_conflict");
    assert!(end["message"].as_str().unwrap().contains("conflict"));
    assert_eq!(std::fs::read_to_string(root.join("conflict")).unwrap(), "harness");
    assert_eq!(std::fs::read_to_string(remote.join("conflict")).unwrap(), "remote\n");
    ssh.server.kill().await.unwrap();
}
