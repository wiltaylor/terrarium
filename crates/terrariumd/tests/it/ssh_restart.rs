//! Real sshd offline completion, shared assertions with fake exe.dev.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common::TestDaemon;
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

pub(crate) struct RemoteCleanup(pub PathBuf);

impl Drop for RemoteCleanup {
    fn drop(&mut self) {
        cleanup(&self.0);
    }
}

pub(crate) async fn start(td: &TestDaemon, target: &str, root: &str) -> (String, String) {
    td.project("p").await;
    let agent = td
        .agent(
            "p",
            "restart-worker",
            json!({"policy":{"targets":[target]}}),
        )
        .await;
    let aid = agent["id"].as_str().unwrap().to_owned();
    let script = format!(
        "echo started >> '{root}/launches'; while [ ! -e '{root}/go' ]; do sleep 0.05; done; echo kept-output; echo kept-error >&2; exit 23"
    );
    let run = td
        .post_as(
            &aid,
            &format!("/api/agents/{aid}/run"),
            json!({"target":target,"cwd":root,"name":"offline-ssh","timeout":"30s","cmd":["sh","-c",script]}),
        )
        .await;
    let sid = run.ok()["session_id"].as_str().unwrap();
    td.post_as(
        &aid,
        &format!("/api/targets/{target}/sessions/{sid}/input"),
        json!({"data":"eA=="}),
    )
    .await
    .err(422, "invalid");
    td.post_as(
        &aid,
        &format!("/api/targets/{target}/exec"),
        json!({"cmd":["true"],"durable":true,"pty":true}),
    )
    .await
    .err(422, "invalid");
    (aid, run.ok()["wait"]["id"].as_str().unwrap().to_owned())
}

pub(crate) async fn started(td: &TestDaemon, wid: &str, root: &Path) {
    for _ in 0..100 {
        if root.join("launches").exists() {
            return;
        }
        let wait = td.get(&format!("/api/waits/{wid}")).await;
        assert_eq!(
            wait.ok()["state"],
            "open",
            "command ended before launch: {}",
            wait.body
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut debug = String::new();
    if let Ok(dirs) = std::fs::read_dir(root.join(".terrarium-runs")) {
        for dir in dirs.flatten() {
            for name in ["out", "err", "pid", "exit"] {
                debug.push_str(&format!(
                    "{name}: {:?}\n",
                    std::fs::read_to_string(dir.path().join(name))
                ));
            }
        }
    }
    panic!("no launch in {}: {debug}", root.display());
}

pub(crate) async fn wait_file(file: &Path) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !file.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}

pub(crate) async fn completed(td: &TestDaemon, aid: &str, wid: &str, physical: &Path) {
    let done = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let wait = td.get(&format!("/api/waits/{wid}")).await;
            if wait.ok()["state"] != "open" {
                break wait.body;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(done["state"], "resolved", "{done}");
    assert_eq!(done["result"]["exit_code"], 23, "{done}");
    assert_eq!(done["result"]["stdout_tail"], "kept-output\n");
    assert_eq!(done["result"]["stderr_tail"], "kept-error\n");
    assert_eq!(
        std::fs::read_to_string(physical.join("launches")).unwrap(),
        "started\n"
    );
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let mail = td.get(&format!("/api/mail?to={aid}&from=system")).await;
            let count = mail
                .ok()
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["body"].as_str().unwrap_or("").contains(wid))
                .count();
            if count > 0 {
                assert_eq!(count, 1);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}

pub(crate) fn cleanup(physical: &Path) {
    let Ok(dirs) = std::fs::read_dir(physical.join(".terrarium-runs")) else {
        return;
    };
    for dir in dirs.flatten() {
        let Ok(pid) = std::fs::read_to_string(dir.path().join("janitor")) else {
            continue;
        };
        // Only the cleanup process group created by this fixture.
        let _ = std::process::Command::new("kill")
            .args(["-KILL", "--", &format!("-{}", pid.trim())])
            .status()
            .unwrap();
    }
}

#[tokio::test]
async fn durable_ssh_finishes_offline_once_with_real_exit() {
    let Some(sshd) = std::env::var_os("TERRARIUM_TEST_SSHD") else {
        return;
    };
    let mut ssh = super::workspace_sync::Ssh::start(sshd).await;
    let _cleanup = RemoteCleanup(ssh.jail.join("tmp/restart"));
    let mut td = TestDaemon::start().await;
    td.post("/api/secrets", json!({"name":"restart-key","kind":"ssh_key","fields":{"private_key":std::fs::read_to_string(&ssh.key).unwrap()}})).await.ok();
    td.post("/api/targets", json!({"name":"restart-ssh","kind":"ssh","address":"root@127.0.0.1","port":ssh.port,"credential":"restart-key","work_root":"/tmp/restart"})).await.ok();
    let (aid, wid) = start(&td, "restart-ssh", "/tmp/restart").await;
    let root = ssh.jail.join("tmp/restart");
    started(&td, &wid, &root).await;
    td.daemon.take().unwrap().shutdown_and_wait().await;
    std::fs::write(root.join("go"), "go").unwrap();
    let spool = std::fs::read_dir(root.join(".terrarium-runs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    wait_file(&spool.join("exit")).await;
    assert_eq!(std::fs::read_to_string(spool.join("exit")).unwrap(), "23\n");
    td.boot(None, None).await;
    completed(&td, &aid, &wid, &root).await;
    td.boot(None, None).await;
    completed(&td, &aid, &wid, &root).await;
    ssh.server.kill().await.unwrap();
}

#[tokio::test]
async fn durable_ssh_redefined_target_is_not_contacted() {
    let Some(sshd) = std::env::var_os("TERRARIUM_TEST_SSHD") else { return; };
    let mut ssh = super::workspace_sync::Ssh::start(sshd).await;
    let root = ssh.jail.join("tmp/redefined");
    let _cleanup = RemoteCleanup(root);
    let mut td = TestDaemon::start().await;
    td.post("/api/secrets", json!({"name":"restart-key","kind":"ssh_key","fields":{"private_key":std::fs::read_to_string(&ssh.key).unwrap()}})).await.ok();
    td.post("/api/targets", json!({"name":"redefined-ssh","kind":"ssh","address":"root@127.0.0.1","port":ssh.port,"credential":"restart-key","work_root":"/tmp/redefined"})).await.ok();
    let opened = td.post("/api/targets/redefined-ssh/exec", json!({"durable":true,"cmd":["sh","-c","echo retained; exit 19"]})).await;
    let sid = opened.ok()["session_id"].as_str().unwrap().to_owned();
    assert_eq!(super::exedev::output(&td, "redefined-ssh", &sid).await.1["code"], 19);
    td.daemon.take().unwrap().shutdown_and_wait().await;
    let conn = terrariumd::db::open(&td.state.join("terrarium.db")).unwrap();
    conn.execute("UPDATE targets SET def=json_set(def,'$.address','root@127.0.0.2') WHERE name='redefined-ssh'", []).unwrap();
    drop(conn);
    td.boot(None, None).await;
    let (_, end) = super::exedev::output(&td, "redefined-ssh", &sid).await;
    assert_eq!(end["code"], "interrupted", "{end}");
    assert!(end["message"].as_str().unwrap().contains("target changed"), "{end}");
    ssh.server.kill().await.unwrap();
}
