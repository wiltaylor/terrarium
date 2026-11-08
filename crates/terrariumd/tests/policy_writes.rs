//! Who may write what into an agent's policy (PLAN §10;
//! `docs/security-review.md` F13): a project_admin sets project targets and
//! `mail_to` in its project; a global target or a `cross_project` entry
//! needs a sysadmin or the operator.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::TestDaemon;
use serde_json::{Value, json};

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

async fn setup() -> (TestDaemon, String, String, String) {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.project("ops").await;
    td.post(
        "/api/targets",
        json!({"name": "global-box", "kind": "ssh", "address": "u@h"}),
    )
    .await
    .ok();
    td.post(
        "/api/targets",
        json!({"name": "p-box", "kind": "ssh", "project": "p", "address": "u@h"}),
    )
    .await
    .ok();
    let member = id(&td.agent("p", "member", json!({})).await);
    let admin = id(&td
        .agent("p", "admin", json!({"roles": ["project_admin"]}))
        .await);
    let sys = id(&td
        .agent("ops", "root", json!({"roles": ["sysadmin"]}))
        .await);
    (td, member, admin, sys)
}

fn def(name: &str, policy: Value) -> Value {
    json!({"name": name, "project": "p", "harness": "claude", "policy": policy})
}

#[tokio::test]
async fn create_by_role() {
    let (td, member, admin, sys) = setup().await;
    let global = json!({"targets": ["global-box"]});
    let cross = json!({"cross_project": ["ops/*"]});
    let outside = json!({"mail_to": ["ops/root"]});
    let local = json!({"targets": ["p-box"], "mail_to": ["*", "admin"]});

    // A member creates no agent at all.
    td.post_as(&member, "/api/agents", def("a", local.clone()))
        .await
        .err(403, "forbidden");
    // A project admin: project targets and mail_to in the project only.
    for (n, p) in [("b", &global), ("c", &cross), ("d", &outside)] {
        let r = td.post_as(&admin, "/api/agents", def(n, p.clone())).await;
        r.err(403, "forbidden");
        assert!(
            r.body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("sysadmin"),
            "{}",
            r.body
        );
    }
    td.post_as(&admin, "/api/agents", def("e", local.clone()))
        .await
        .ok();
    // A sysadmin and the operator may grant all of it.
    td.post_as(&sys, "/api/agents", def("f", global.clone()))
        .await
        .ok();
    td.post_as(&sys, "/api/agents", def("g", cross.clone()))
        .await
        .ok();
    td.post("/api/agents", def("h", global)).await.ok();
    td.post("/api/agents", def("i", cross)).await.ok();
}

#[tokio::test]
async fn update_by_role() {
    let (td, member, admin, sys) = setup().await;
    let worker = id(&td.agent("p", "worker", json!({})).await);
    let path = format!("/api/agents/{worker}");

    td.patch_as(&member, &path, json!({"policy": {"targets": ["p-box"]}}))
        .await
        .err(403, "forbidden");
    td.patch_as(
        &admin,
        &path,
        json!({"policy": {"targets": ["global-box"]}}),
    )
    .await
    .err(403, "forbidden");
    td.patch_as(
        &admin,
        &path,
        json!({"policy": {"cross_project": ["ops/root"]}}),
    )
    .await
    .err(403, "forbidden");
    td.patch_as(
        &admin,
        &path,
        json!({"policy": {"targets": ["p-box"], "mail_to": ["admin"]}}),
    )
    .await
    .ok();

    // A project admin cannot widen its own policy either.
    td.patch_as(
        &admin,
        &format!("/api/agents/{admin}"),
        json!({"policy": {"targets": ["global-box"]}}),
    )
    .await
    .err(403, "forbidden");

    // A sysadmin grants the global target; the project admin may then
    // change the rest and keep it, but not add another cross entry.
    td.patch_as(
        &sys,
        &path,
        json!({"policy": {"targets": ["p-box", "global-box"], "cross_project": ["ops/root"]}}),
    )
    .await
    .ok();
    let v = td
        .patch_as(
            &admin,
            &path,
            json!({"policy": {"targets": ["global-box"], "mail_to": ["*"],
                              "cross_project": ["ops/root"]}}),
        )
        .await;
    assert_eq!(v.ok()["policy"]["targets"], json!(["global-box"]));
    td.patch_as(
        &admin,
        &path,
        json!({"policy": {"targets": ["global-box"], "cross_project": ["ops/root", "ops/*"]}}),
    )
    .await
    .err(403, "forbidden");
    // The operator may do anything.
    td.patch(&path, json!({"policy": {"cross_project": ["ops/*"]}}))
        .await
        .ok();
}
