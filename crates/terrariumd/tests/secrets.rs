//! The secret manager (PLAN §16b, docs/api.md §5.2): the API, its rules,
//! the reveal, the project scope of a repo's secret, and the move of the
//! old credential files at the start. No answer and no audit row holds a
//! secret value, except the reveal.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::TestDaemon;
use serde_json::{Value, json};

/// Each secret value that the tests store. No answer may hold one.
const VALUES: [&str; 6] = [
    "ghp_GLOBALs3cret",
    "sk-PROJECTs3cret",
    "pin-SECRET-4321",
    "pass WORD s3cret",
    "ghp_PROJECTtoken",
    "ghp_REPLACEDs3cret",
];

fn clean(v: &Value) {
    let text = v.to_string();
    for s in VALUES {
        assert!(!text.contains(s), "an answer holds `{s}`: {text}");
    }
}

#[tokio::test]
async fn secrets_store_list_and_never_answer_a_value() {
    let td = TestDaemon::start().await;
    td.project("p").await;

    let r = td
        .post(
            "/api/secrets",
            json!({"name": "gh", "kind": "token",
                   "fields": {"value": VALUES[0], "hosts": ["github.com"]}}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 201);
    clean(&r.body);
    assert_eq!(r.body["scope"], "global");
    assert_eq!(r.body["fields"]["username"], "x-access-token");
    assert_eq!(r.body["secret_fields"], json!(["value"]));
    assert_eq!(r.body["created_by"], "operator");

    let r = td
        .post(
            "/api/secrets",
            json!({"scope": "p", "name": "openai", "kind": "api_key",
                   "fields": {"value": VALUES[1], "env": "OPENAI_API_KEY"}}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 201);
    clean(&r.body);
    let r = td
        .put(
            "/api/secrets/p/door",
            json!({"kind": "generic", "fields": {"user": "bob", "pin": VALUES[2]},
                   "secret_fields": ["pin"]}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 201);
    assert_eq!(r.body["fields"], json!({"user": "bob"}));
    let r = td
        .put(
            "/api/secrets/global/win",
            json!({"kind": "login", "fields": {"username": "Admin", "password": VALUES[3]}}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 201);

    // A PUT of an existing secret replaces it: 200, the same ID.
    let id = td.get("/api/secrets/global/gh").await.ok()["id"].clone();
    let r = td
        .put(
            "/api/secrets/global/gh",
            json!({"kind": "token", "fields": {"value": VALUES[5]}}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 200);
    assert_eq!(r.body["id"], id);
    clean(&r.body);
    // A POST of an existing name is a conflict.
    td.post(
        "/api/secrets",
        json!({"name": "gh", "kind": "token", "fields": {"value": "x"}}),
    )
    .await
    .err(409, "conflict");
    // Bad bodies.
    td.post(
        "/api/secrets",
        json!({"name": "k", "kind": "ssh_key", "fields": {"private_key": "nope"}}),
    )
    .await
    .err(422, "invalid");
    td.post(
        "/api/secrets",
        json!({"scope": "nope", "name": "k", "kind": "api_key", "fields": {"value": "v"}}),
    )
    .await
    .err(404, "not_found");
    td.post(
        "/api/secrets",
        json!({"name": "k", "kind": "password", "fields": {"value": "v"}}),
    )
    .await
    .err(400, "bad_request");

    // The lists: all for the operator, one scope with `?scope=`.
    let all = td.get("/api/secrets").await;
    clean(&all.body);
    let names: Vec<String> = all
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            format!(
                "{}/{}",
                s["scope"].as_str().unwrap(),
                s["name"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(names, ["global/gh", "global/win", "p/door", "p/openai"]);
    let p = td.get("/api/secrets?scope=p").await;
    assert_eq!(p.ok().as_array().unwrap().len(), 2);
    clean(&td.get("/api/secrets/p/door").await.body);
    td.get("/api/secrets/p/none").await.err(404, "not_found");

    // The alias: global secrets in the old shape; a login is a `password`.
    let creds = td.get("/api/credentials").await;
    clean(&creds.body);
    assert_eq!(
        creds.body,
        json!([
            {"name": "gh", "kind": "token", "username": "x-access-token",
             "created_at": creds.body[0]["created_at"]},
            {"name": "win", "kind": "password", "username": "Admin",
             "created_at": creds.body[1]["created_at"]},
        ])
    );

    // Delete.
    let r = td.delete("/api/secrets/p/door").await;
    assert_eq!(r.status.as_u16(), 204);
    td.delete("/api/secrets/p/door").await.err(404, "not_found");

    // The audit rows name the secrets and hold no value.
    let audit = td.get("/api/audit?action=secret.put").await;
    assert_eq!(audit.ok().as_array().unwrap().len(), 5);
    clean(&audit.body);
    let audit = td.get("/api/audit?action=secret.delete").await;
    assert_eq!(audit.ok()[0]["target"], "p/door");
    assert_eq!(audit.body[0]["project"], "p");

    // The database file holds no value either.
    let db = std::fs::read(td.state.join("terrarium.db")).unwrap();
    let wal = std::fs::read(td.state.join("terrarium.db-wal")).unwrap_or_default();
    for s in VALUES {
        for bytes in [&db, &wal] {
            assert!(
                !bytes.windows(s.len()).any(|w| w == s.as_bytes()),
                "the database holds `{s}`"
            );
        }
    }
}

#[tokio::test]
async fn secrets_follow_the_roles_of_their_scope() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.project("q").await;
    td.agent("p", "w", json!({})).await;
    td.agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await;
    td.agent("q", "root", json!({"roles": ["sysadmin"]})).await;
    let token = |v: &str| json!({"kind": "token", "fields": {"value": v}});

    // A member: nothing.
    td.get_as("p/w", "/api/secrets").await.err(403, "forbidden");
    td.put_as("p/w", "/api/secrets/p/x", token("v"))
        .await
        .err(403, "forbidden");
    td.get_as("p/w", "/api/credentials")
        .await
        .err(403, "forbidden");

    // A project_admin: its own project only.
    let r = td.put_as("p/lead", "/api/secrets/p/x", token("v")).await;
    assert_eq!(r.status.as_u16(), 201);
    assert!(r.body["created_by"].as_str().unwrap().starts_with("agent:"));
    td.put_as("p/lead", "/api/secrets/q/x", token("v"))
        .await
        .err(403, "forbidden");
    td.put_as("p/lead", "/api/secrets/global/x", token("v"))
        .await
        .err(403, "forbidden");
    td.post_as(
        "p/lead",
        "/api/credentials",
        json!({"name": "x", "kind": "token", "value": "v"}),
    )
    .await
    .err(403, "forbidden");
    // Its list without `?scope=` is its own project.
    td.put("/api/secrets/global/g", token("v")).await;
    let l = td.get_as("p/lead", "/api/secrets").await;
    assert_eq!(l.ok().as_array().unwrap().len(), 1);
    assert_eq!(l.body[0]["scope"], "p");
    td.get_as("p/lead", "/api/secrets?scope=global")
        .await
        .err(403, "forbidden");

    // A sysadmin: every scope, but no reveal.
    let r = td
        .put_as("q/root", "/api/secrets/global/y", token("v"))
        .await;
    assert_eq!(r.status.as_u16(), 201);
    assert_eq!(
        td.get_as("q/root", "/api/secrets")
            .await
            .ok()
            .as_array()
            .unwrap()
            .len(),
        3
    );
    td.post_as(
        "q/root",
        "/api/secrets/global/y/reveal",
        json!({"password": "x"}),
    )
    .await
    .err(403, "forbidden");
    let r = td
        .req(
            hyper::Method::DELETE,
            "/api/secrets/p/x",
            None,
            Some("q/root"),
        )
        .await;
    assert_eq!(r.status.as_u16(), 204);

    // A project delete deletes its secrets.
    td.project("r").await;
    td.put("/api/secrets/r/z", token("v")).await;
    let r = td.delete("/api/projects/r?confirm=true").await;
    assert_eq!(r.status.as_u16(), 204, "{:?}", r.body);
    assert_eq!(
        td.get("/api/secrets?scope=global")
            .await
            .ok()
            .as_array()
            .unwrap()
            .len(),
        2
    );
    td.get("/api/secrets/r/z").await.err(404, "not_found");
}

#[tokio::test]
async fn a_reveal_needs_the_operator_password_and_is_audited() {
    let td = TestDaemon::start().await;
    td.put(
        "/api/secrets/global/gh",
        json!({"kind": "token", "fields": {"value": VALUES[0], "username": "bot"}}),
    )
    .await;
    // No password set: the reveal cannot check one.
    td.post(
        "/api/secrets/global/gh/reveal",
        json!({"password": "anything"}),
    )
    .await
    .err(409, "conflict");
    terrariumd::password::PasswordStore::new(&td.state)
        .set("correct horse battery")
        .unwrap();
    td.post("/api/secrets/global/gh/reveal", json!({}))
        .await
        .err(422, "invalid");
    td.post(
        "/api/secrets/global/gh/reveal",
        json!({"password": "wrong"}),
    )
    .await
    .err(403, "forbidden");
    // The wrong password counts as a failed login.
    let failed = td.get("/api/audit?action=auth.login_failed").await;
    assert_eq!(failed.ok().as_array().unwrap().len(), 1);
    td.post(
        "/api/secrets/global/none/reveal",
        json!({"password": "correct horse battery"}),
    )
    .await
    .err(404, "not_found");

    let r = td
        .post(
            "/api/secrets/global/gh/reveal",
            json!({"password": "correct horse battery"}),
        )
        .await;
    assert_eq!(r.ok()["values"], json!({"value": VALUES[0]}));
    assert_eq!(r.body["fields"]["username"], "bot");
    assert_eq!(r.headers["cache-control"], "no-store");

    let audit = td.get("/api/audit?action=secret.reveal").await;
    assert_eq!(audit.ok().as_array().unwrap().len(), 1);
    assert_eq!(audit.body[0]["target"], "global/gh");
    clean(&audit.body);
}

#[tokio::test]
async fn a_repo_uses_its_projects_secret_before_the_global_one() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.project("q").await;
    td.agent("p", "w", json!({})).await;
    td.agent("q", "w", json!({})).await;
    for p in ["p", "q"] {
        td.post(
            &format!("/api/projects/{p}/repos"),
            json!({"name": "hub", "url": "https://git.example.com/team/hub.git", "credential": "hub"}),
        )
        .await
        .ok();
    }
    td.put(
        "/api/secrets/global/hub",
        json!({"kind": "token", "fields": {"value": VALUES[0]}}),
    )
    .await;
    td.put(
        "/api/secrets/p/hub",
        json!({"kind": "github_token", "fields": {"value": VALUES[4], "host": "git.example.com"}}),
    )
    .await;
    let ask = json!({"protocol": "https", "host": "git.example.com", "path": "team/hub.git"});
    let r = td
        .post_as("p/w", "/api/repos/credential", ask.clone())
        .await;
    assert_eq!(r.ok()["password"], VALUES[4]);
    let r = td
        .post_as("q/w", "/api/repos/credential", ask.clone())
        .await;
    assert_eq!(r.ok()["password"], VALUES[0]);
    // An api_key is no git credential.
    td.put(
        "/api/secrets/q/hub",
        json!({"kind": "api_key", "fields": {"value": "sk-x"}}),
    )
    .await;
    td.post_as("q/w", "/api/repos/credential", ask)
        .await
        .err(404, "not_found");
}

#[tokio::test]
async fn old_credential_files_move_in_at_the_start() {
    let mut td = TestDaemon::start().await;
    let dir = td.state.join("credentials");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("deploy"), "ghp_old\n").unwrap();
    std::fs::write(
        dir.join("deploy.json"),
        json!({"name": "deploy", "kind": "token", "username": "bot",
               "created_at": "2026-01-01T00:00:00.000Z"})
        .to_string(),
    )
    .unwrap();
    std::fs::write(dir.join("vnc"), " pw ").unwrap();
    std::fs::write(
        dir.join("vnc.json"),
        json!({"name": "vnc", "kind": "password"}).to_string(),
    )
    .unwrap();
    td.boot(None, None).await;

    let creds = td.get("/api/credentials").await;
    assert_eq!(creds.ok()[0]["name"], "deploy");
    assert_eq!(creds.body[0]["username"], "bot");
    assert_eq!(creds.body[0]["created_at"], "2026-01-01T00:00:00.000Z");
    assert_eq!(creds.body[1]["kind"], "password");
    assert_eq!(
        td.get("/api/secrets/global/vnc").await.ok()["kind"],
        "login"
    );
    assert!(!dir.join("deploy").exists() && !dir.join("deploy.json").exists());
    assert_eq!(std::fs::read_dir(dir.join("backup")).unwrap().count(), 4);
    let audit = td.get("/api/audit?action=secret.migrated").await;
    assert_eq!(audit.ok().as_array().unwrap().len(), 2);
    assert!(!audit.body.to_string().contains("ghp_old"));

    // The next start moves nothing.
    td.boot(None, None).await;
    let audit = td.get("/api/audit?action=secret.migrated").await;
    assert_eq!(audit.ok().as_array().unwrap().len(), 2);
}
