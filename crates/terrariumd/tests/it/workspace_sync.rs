//! Refuse workspace sync rather than creating the sandbox path on SSH VMs.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common::TestDaemon;
use serde_json::json;

#[tokio::test]
async fn workspace_sync_is_refused_for_ssh_and_exedev() {
    let td = TestDaemon::start().await;
    for kind in ["ssh", "exedev"] {
        let response = td.post("/api/targets", json!({
            "name": "sync", "kind": kind, "address": "user@vm",
            "work_root": "/home/user/work", "sync_workspace": true
        })).await;
        response.err(422, "invalid");
        assert!(response.body.to_string().contains("disable it"));
        assert!(response.body.to_string().contains("work_root"));
    }
    td.post("/api/targets", json!({"name":"plain", "kind":"ssh", "address":"user@vm"})).await.ok();
    td.patch("/api/targets/plain", json!({"sync_workspace":true})).await.err(422, "invalid");
}
