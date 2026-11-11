//! Real sshd offline completion, shared assertions with fake exe.dev.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common::TestDaemon;
use serde_json::json;
use std::{path::Path, time::Duration};

pub(crate) async fn start(td: &TestDaemon, target: &str, root: &str) -> (String, String) {
    td.project("p").await;
    let agent = td.agent("p", "restart-worker", json!({"policy":{"targets":[target]}})).await;
    let aid = agent["id"].as_str().unwrap().to_owned();
    let script = format!("echo started >> '{root}/launches'; while [ ! -e '{root}/go' ]; do sleep 0.05; done; echo kept-output; echo kept-error >&2; exit 23");
    let run = td.post_as(&aid, &format!("/api/agents/{aid}/run"), json!({"target":target,"name":"offline-ssh","cmd":["sh","-c",script]})).await;
    (aid, run.ok()["wait"]["id"].as_str().unwrap().to_owned())
}

pub(crate) async fn wait_file(file: &Path) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !file.exists() { tokio::time::sleep(Duration::from_millis(20)).await; }
    }).await.unwrap();
}

pub(crate) async fn completed(td: &TestDaemon, aid: &str, wid: &str, physical: &Path) {
    let done = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let wait = td.get(&format!("/api/waits/{wid}")).await;
            if wait.ok()["state"] != "open" { break wait.body; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.unwrap();
    assert_eq!(done["state"], "resolved", "{done}");
    assert_eq!(done["result"]["exit_code"], 23, "{done}");
    assert_eq!(done["result"]["stdout_tail"], "kept-output\n");
    assert_eq!(done["result"]["stderr_tail"], "kept-error\n");
    assert_eq!(std::fs::read_to_string(physical.join("launches")).unwrap(), "started\n");
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let mail = td.get(&format!("/api/mail?to={aid}&from=system")).await;
            let count = mail.ok().as_array().unwrap().iter()
                .filter(|m| m["body"].as_str().unwrap_or("").contains(wid)).count();
            if count > 0 { assert_eq!(count, 1); break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.unwrap();
}

pub(crate) fn cleanup(physical: &Path) {
    for dir in std::fs::read_dir(physical.join(".terrarium-runs")).unwrap() {
        let pid = std::fs::read_to_string(dir.unwrap().path().join("janitor")).unwrap();
        // Only the cleanup process group created by this fixture.
        let status = std::process::Command::new("kill").args(["-KILL", "--", &format!("-{}",pid.trim())]).status().unwrap();
        assert!(status.success());
    }
}

#[tokio::test]
async fn durable_ssh_finishes_offline_once_with_real_exit() {
    let Some(sshd) = std::env::var_os("TERRARIUM_TEST_SSHD") else { return };
    let mut ssh = super::workspace_sync::Ssh::start(sshd).await;
    let mut td = TestDaemon::start().await;
    td.post("/api/secrets", json!({"name":"restart-key","kind":"ssh_key","fields":{"private_key":std::fs::read_to_string(&ssh.key).unwrap()}})).await.ok();
    td.post("/api/targets", json!({"name":"restart-ssh","kind":"ssh","address":"root@127.0.0.1","port":ssh.port,"credential":"restart-key","work_root":"/tmp/restart"})).await.ok();
    let (aid, wid) = start(&td, "restart-ssh", "/tmp/restart").await;
    let root = ssh.jail.join("tmp/restart");
    wait_file(&root.join("launches")).await;
    td.daemon.take().unwrap().shutdown_and_wait().await;
    std::fs::write(root.join("go"), "go").unwrap();
    let spool = std::fs::read_dir(root.join(".terrarium-runs")).unwrap().next().unwrap().unwrap().path();
    wait_file(&spool.join("exit")).await;
    assert_eq!(std::fs::read_to_string(spool.join("exit")).unwrap(), "23\n");
    td.boot(None,None).await;
    completed(&td, &aid, &wid, &root).await;
    cleanup(&root);
    ssh.server.kill().await.unwrap();
}
