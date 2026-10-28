//! A project object names a global secret (PLAN §16b.6a, docs/api.md
//! §5.1, §5.2, §17.2): a project repo or a project target can name a
//! global secret only when the project holds a grant of it, or when the
//! operator or a sysadmin set the name. The daemon checks at save and at
//! each use. Also the rule of §16b.3 that a `use` grant of a token gives
//! the value to the agent (the credential helper hands it to git).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::TestDaemon;
use serde_json::{Value, json};

const TOKEN: &str = "ghp_GLOBALruleTOKEN1234";

async fn setup() -> TestDaemon {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.project("q").await;
    td.agent("p", "w", json!({})).await;
    td.agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await;
    td.agent("q", "root", json!({"roles": ["sysadmin"]})).await;
    td.put(
        "/api/secrets/global/gh",
        json!({"kind": "token", "fields": {"value": TOKEN}}),
    )
    .await
    .ok();
    let r = td.post("/api/secrets/global/key/generate", json!({})).await;
    assert_eq!(r.status.as_u16(), 201, "{}", r.body);
    td
}

fn repo(name: &str, host: &str) -> Value {
    json!({"name": name, "url": format!("https://{host}/x/{name}.git"), "credential": "gh"})
}

fn target(name: &str) -> Value {
    json!({"name": name, "kind": "ssh", "project": "p", "address": "build@box",
           "credential": "key"})
}

async fn grant(td: &TestDaemon, secret: &str) {
    td.post(
        &format!("/api/secrets/global/{secret}/grants"),
        json!({"grantee": "project:p", "level": "use"}),
    )
    .await
    .ok();
}

async fn revoke(td: &TestDaemon, secret: &str) {
    let r = td
        .delete(&format!(
            "/api/secrets/global/{secret}/grants?grantee=project:p"
        ))
        .await;
    assert_eq!(r.status.as_u16(), 204, "{}", r.body);
}

async fn refused_events(td: &TestDaemon) -> Vec<Value> {
    // The event write runs after the answer.
    for _ in 0..50 {
        let r = td.get("/api/events?kind=secret.refused").await;
        let rows = r.ok().as_array().unwrap().clone();
        if !rows.is_empty() {
            return rows;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    Vec::new()
}

#[tokio::test]
async fn a_project_admin_needs_a_grant_to_name_a_global_secret() {
    let td = setup().await;
    // A project_admin: 403 with the rule and the fix.
    let r = td
        .post_as("p/lead", "/api/projects/p/repos", repo("a", "evil.example"))
        .await;
    r.err(403, "forbidden");
    let m = r.body["error"]["message"].as_str().unwrap_or_default();
    assert!(m.contains("terrarium secret grant gh project:p"), "{m}");
    td.post_as("p/lead", "/api/targets", target("t1"))
        .await
        .err(403, "forbidden");

    // The operator and a sysadmin: no grant needed.
    let r = td
        .post("/api/projects/p/repos", repo("b", "git.example"))
        .await;
    assert_eq!(r.ok()["credential_admin"], true);
    let r = td
        .post_as("q/root", "/api/projects/p/repos", repo("c", "git.example"))
        .await;
    assert_eq!(r.ok()["credential_admin"], true);
    let r = td.post_as("q/root", "/api/targets", target("t2")).await;
    assert_eq!(r.status.as_u16(), 201, "{}", r.body);

    // A grant to the project: the project_admin can name it.
    grant(&td, "gh").await;
    grant(&td, "key").await;
    let r = td
        .post_as("p/lead", "/api/projects/p/repos", repo("d", "git.example"))
        .await;
    assert_eq!(r.ok()["credential_admin"], false);
    let r = td.post_as("p/lead", "/api/targets", target("t3")).await;
    assert_eq!(r.status.as_u16(), 201, "{}", r.body);

    // A project secret of the same name wins, and needs no grant.
    td.put(
        "/api/secrets/q/gh",
        json!({"kind": "token", "fields": {"value": "ghp_PROJECTq9999"}}),
    )
    .await
    .ok();
    td.agent("q", "qlead", json!({"roles": ["project_admin"]}))
        .await;
    let r = td
        .post_as("q/qlead", "/api/projects/q/repos", repo("e", "git.example"))
        .await;
    assert_eq!(r.status.as_u16(), 201, "{}", r.body);
}

#[tokio::test]
async fn a_revoke_stops_the_use_of_a_project_admin_object() {
    let td = setup().await;
    grant(&td, "gh").await;
    grant(&td, "key").await;
    td.post_as("p/lead", "/api/projects/p/repos", repo("a", "evil.example"))
        .await
        .ok();
    td.post("/api/projects/p/repos", repo("b", "git.example"))
        .await
        .ok();
    td.post_as("p/lead", "/api/targets", target("t1"))
        .await
        .ok();
    let ask = |host: &str, name: &str| json!({"protocol": "https", "host": host, "path": format!("x/{name}.git")});
    // With the grant, the helper answers for the project_admin's repo.
    let r = td
        .post_as("p/w", "/api/repos/credential", ask("evil.example", "a"))
        .await;
    assert_eq!(r.ok()["password"], TOKEN);

    revoke(&td, "gh").await;
    revoke(&td, "key").await;
    // The reviewer's attack: a repo on a host that the project_admin
    // controls, a global token, no grant. The helper gives nothing.
    td.post_as("p/w", "/api/repos/credential", ask("evil.example", "a"))
        .await
        .err(404, "not_found");
    // The operator's repo still works: an admin set its credential.
    let r = td
        .post_as("p/w", "/api/repos/credential", ask("git.example", "b"))
        .await;
    assert_eq!(r.ok()["password"], TOKEN);
    // The project_admin's target: the test is refused, and the view warns.
    td.post_empty("/api/targets/p-t1/test")
        .await
        .err(403, "forbidden");
    let v = td.get("/api/targets/p-t1").await;
    let w = v.ok()["credential_info"]["warning"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(w.contains("has no grant"), "{w}");

    let events = refused_events(&td).await;
    assert!(
        events
            .iter()
            .any(|e| e["data"]["object"] == "repo p/a" && e["data"]["level"] == "warning"),
        "{events:#?}"
    );
    assert!(!td.get("/api/events").await.body.to_string().contains(TOKEN));
}

#[tokio::test]
async fn a_project_admin_cannot_move_an_operator_target() {
    let td = setup().await;
    let r = td.post("/api/targets", target("t1")).await;
    assert_eq!(r.status.as_u16(), 201, "{}", r.body);
    // `keep` alone keeps the operator's mark.
    let r = td
        .patch_as("p/lead", "/api/targets/p-t1", json!({"keep": true}))
        .await;
    assert_eq!(r.status.as_u16(), 200, "{}", r.body);
    // A new address with the global key: a grant is necessary.
    td.patch_as(
        "p/lead",
        "/api/targets/p-t1",
        json!({"address": "root@evil.example"}),
    )
    .await
    .err(403, "forbidden");
}

/// PLAN §16b.3: git's credential helper hands the token to git, so an
/// agent with a `use` grant of a `token` can read the value. This is the
/// intended behaviour (Wil, 2026-10-29: "use = read for tokens"); a real
/// fix is an HTTPS proxy in the daemon. Do not change it without that.
#[tokio::test]
async fn a_use_grant_of_a_token_gives_the_value_to_the_agent() {
    let td = setup().await;
    td.put(
        "/api/secrets/global/gh",
        json!({"kind": "token", "fields": {"value": TOKEN, "hosts": ["git.example"]}}),
    )
    .await
    .ok();
    td.post(
        "/api/secrets/global/gh/grants",
        json!({"grantee": "agent:p/w", "level": "use"}),
    )
    .await
    .ok();
    let r = td
        .post_as(
            "p/w",
            "/api/repos/credential",
            json!({"protocol": "https", "host": "git.example"}),
        )
        .await;
    assert_eq!(r.ok()["password"], TOKEN);
}
