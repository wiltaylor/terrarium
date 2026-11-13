//! A live host process, not a synthetic event, crosses a daemon restart.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common::TestDaemon;
use serde_json::json;
use std::time::Duration;
use terrarium_host::{Client, ClientOptions};

#[tokio::test]
async fn native_merge_keeps_recipe_exit_tail_slot_and_junit_across_restart() {
    crate::jobs::native_merge_restart_keeps_recipe_exit_tail_and_junit(false).await;
}

#[tokio::test]
async fn native_merge_finishes_while_daemon_is_offline() {
    crate::jobs::native_merge_restart_keeps_recipe_exit_tail_and_junit(true).await;
}

pub(crate) async fn wait_pid_gone(pid: &str) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while std::path::Path::new(&format!("/proc/{}", pid.trim())).exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}

async fn online(td: &TestDaemon) {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let host = td.get("/api/hosts/restart-host").await;
            if host.body["online"] == true {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn native_process_wait_retains_real_exit_and_tail_across_daemon_restart() {
    process_restart(false).await;
}

#[tokio::test]
async fn native_process_wait_finishes_while_daemon_is_offline() {
    process_restart(true).await;
}

async fn process_restart(offline: bool) {
    let mut td = TestDaemon::start().await;
    td.project("p").await;
    let agent = td
        .agent("p", "worker", json!({"policy": {"targets": ["p-box"]}}))
        .await;
    let aid = agent["id"].as_str().unwrap();
    let token = td.post("/api/hosts", json!({"name": "restart-host"})).await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    let (listener, addr) = terrariumd::hosts::serve(state.clone(), "127.0.0.1:0")
        .await
        .unwrap();
    let mut opts = ClientOptions::new(
        format!("ws://{addr}/hosts/connect"),
        token.ok()["token"].as_str().unwrap(),
        "restart-host",
    );
    opts.backoff_min = Duration::from_millis(50);
    opts.backoff_max = Duration::from_millis(100);
    opts.capabilities.shell = Some("/bin/sh".into());
    let client = tokio::spawn(Client::new(opts).run());
    online(&td).await;
    td.post(
        "/api/targets",
        json!({"name": "p-box", "kind": "host", "host": "restart-host", "project": "p"}),
    )
    .await
    .ok();
    let go = td.dir.path().join("go");
    let pidfile = td.dir.path().join("pid");
    let script = format!(
        "echo $$ > '{}'; echo before; while [ ! -e '{}' ]; do sleep 0.02; done; echo after; echo kept-error >&2; exit 23",
        pidfile.display(),
        go.display()
    );
    let run = td
        .post_as(
            aid,
            &format!("/api/agents/{aid}/run"),
            json!({"target": "p-box", "name": "restart", "cmd": ["sh", "-c", script]}),
        )
        .await;
    let wid = run.ok()["wait"]["id"].as_str().unwrap().to_owned();
    let sid = run.body["session_id"].as_str().unwrap().to_owned();
    // The shell creates the pid file before `echo $$` writes into it, so
    // wait for a full line, not for the file to exist (TER-1166).
    let pid = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(text) = std::fs::read_to_string(&pidfile)
                && text.len() > 1
                && text.ends_with('\n')
            {
                return text;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("pid file holds a full line within 30s");
    listener.abort();
    let _ = listener.await;
    if offline {
        td.daemon.take().unwrap().shutdown_and_wait().await;
        std::fs::write(&go, b"go").unwrap();
        wait_pid_gone(&pid).await;
    }
    td.boot(None, None).await;
    let new_state = td.daemon.as_ref().unwrap().state.clone();
    assert!(!std::sync::Arc::ptr_eq(
        &state.executor,
        &new_state.executor
    ));
    let (listener, _) = terrariumd::hosts::serve(new_state, &addr.to_string())
        .await
        .unwrap();
    online(&td).await;
    assert_eq!(std::fs::read_to_string(&pidfile).unwrap(), pid);
    if !offline {
        assert!(std::path::Path::new(&format!("/proc/{}", pid.trim())).exists());
        std::fs::write(go, b"go").unwrap();
    }
    let done = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let wait = td.get(&format!("/api/waits/{wid}")).await;
            if wait.ok()["state"] != "open" {
                return wait.body;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(done["state"], "resolved", "{done}");
    assert_eq!(done["result"]["exit_code"], 23, "{done}");
    assert_eq!(done["result"]["stdout_tail"], "before\nafter\n");
    assert_eq!(done["result"]["stderr_tail"], "kept-error\n");
    let log = td.get_as(aid, &format!("/api/waits/{wid}/log")).await;
    assert_eq!(log.ok()["running"], false);
    assert!(log.body["output"].as_str().unwrap().contains("before"));
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let mail = td.get(&format!("/api/mail?to={aid}&from=system")).await;
            let count = mail
                .ok()
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["body"].as_str().unwrap_or("").contains(&wid))
                .count();
            if count > 0 {
                assert_eq!(count, 1, "{}", mail.body);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    td.delete(&format!("/api/targets/p-box/sessions/{sid}"))
        .await
        .ok();
    client.abort();
    listener.abort();
}
