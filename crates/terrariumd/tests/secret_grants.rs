//! Grants of secrets (PLAN §16b.3, §16b.4, docs/api.md §5.2): who may
//! grant, `env` needs `read`, `secret get` needs `read`, a revoke applies
//! at the next call, the git credential helper for a granted host, and the
//! audit rows. No answer except a get holds a value.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::TestDaemon;
use serde_json::{Value, json};

const GLOBAL_VALUE: &str = "sk-GLOBALgrant1234";
const PROJECT_VALUE: &str = "sk-PROJECTgrant5678";
const TOKEN_VALUE: &str = "ghp_GRANTEDtoken9999";

fn clean(v: &Value) {
    let text = v.to_string();
    for s in [GLOBAL_VALUE, PROJECT_VALUE, TOKEN_VALUE] {
        assert!(!text.contains(s), "an answer holds `{s}`: {text}");
    }
}

async fn setup() -> TestDaemon {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.project("q").await;
    td.agent("p", "w", json!({})).await;
    td.agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await;
    td.agent("q", "x", json!({})).await;
    td.agent("q", "root", json!({"roles": ["sysadmin"]})).await;
    td.put(
        "/api/secrets/global/openai",
        json!({"kind": "api_key", "fields": {"value": GLOBAL_VALUE}}),
    )
    .await
    .ok();
    td.put(
        "/api/secrets/p/db",
        json!({"kind": "api_key", "fields": {"value": PROJECT_VALUE}}),
    )
    .await
    .ok();
    td.put(
        "/api/secrets/global/gh",
        json!({"kind": "github_token", "fields": {"value": TOKEN_VALUE, "host": "git.example.org"}}),
    )
    .await
    .ok();
    td
}

#[tokio::test]
async fn grants_follow_the_scope_rules() {
    let td = setup().await;
    let grant = |g: &str| json!({"grantee": g});
    // A global secret: the operator and a sysadmin, to any agent or project.
    let r = td
        .post("/api/secrets/global/openai/grants", grant("agent:q/x"))
        .await;
    assert_eq!(r.status.as_u16(), 201, "{:?}", r.body);
    assert_eq!(r.body["grantee_label"], "agent:q/x");
    assert_eq!(r.body["level"], "read");
    clean(&r.body);
    let r = td
        .post_as(
            "q/root",
            "/api/secrets/global/openai/grants",
            grant("project:p"),
        )
        .await;
    assert_eq!(r.status.as_u16(), 201, "{:?}", r.body);
    // The same grantee again replaces the grant: 200.
    let r = td
        .post("/api/secrets/global/openai/grants", grant("project:p"))
        .await;
    assert_eq!(r.status.as_u16(), 200);
    // A project_admin and a member cannot grant a global secret.
    td.post_as(
        "p/lead",
        "/api/secrets/global/openai/grants",
        grant("agent:p/w"),
    )
    .await
    .err(403, "forbidden");
    td.post_as(
        "p/w",
        "/api/secrets/global/openai/grants",
        grant("agent:p/w"),
    )
    .await
    .err(403, "forbidden");
    // A project secret: its project_admin, to its own project only.
    td.post_as("p/lead", "/api/secrets/p/db/grants", grant("agent:p/w"))
        .await
        .ok();
    td.post_as("p/lead", "/api/secrets/p/db/grants", grant("agent:q/x"))
        .await
        .err(403, "forbidden");
    td.post("/api/secrets/p/db/grants", grant("project:q"))
        .await
        .err(403, "forbidden");
    td.post_as("q/x", "/api/secrets/p/db/grants", grant("agent:q/x"))
        .await
        .err(403, "forbidden");
    // Bad grantees and unknown secrets.
    td.post("/api/secrets/p/db/grants", grant("p/w"))
        .await
        .err(422, "invalid");
    td.post("/api/secrets/p/db/grants", grant("agent:p/none"))
        .await
        .err(404, "not_found");
    td.post("/api/secrets/p/none/grants", grant("agent:p/w"))
        .await
        .err(404, "not_found");
    // The list.
    let l = td.get_as("p/lead", "/api/secrets/p/db/grants").await;
    let labels: Vec<&str> = l
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["grantee_label"].as_str().unwrap())
        .collect();
    assert_eq!(labels, ["agent:p/w"]);
    td.get_as("p/w", "/api/secrets/p/db/grants")
        .await
        .err(403, "forbidden");
    // The audit holds the changes, with no value.
    let audit = td.get("/api/audit?action=secret.grant").await;
    assert!(audit.ok().as_array().unwrap().len() >= 4);
    clean(&audit.body);
}

#[tokio::test]
async fn env_and_get_need_read() {
    let td = setup().await;
    // `env` with `use` is refused; `env` with `read` is good.
    td.post(
        "/api/secrets/global/gh/grants",
        json!({"grantee": "agent:p/w", "env": "GITHUB_TOKEN"}),
    )
    .await
    .err(422, "invalid");
    td.post(
        "/api/secrets/global/gh/grants",
        json!({"grantee": "agent:p/w", "level": "use", "env": "GH"}),
    )
    .await
    .err(422, "invalid");
    td.post(
        "/api/secrets/p/db/grants",
        json!({"grantee": "agent:p/w", "level": "read", "env": "LD_PRELOAD"}),
    )
    .await
    .err(422, "invalid");
    td.post(
        "/api/secrets/p/db/grants",
        json!({"grantee": "agent:p/w", "level": "read", "env": "DB_KEY"}),
    )
    .await
    .ok();
    td.post(
        "/api/secrets/global/gh/grants",
        json!({"grantee": "project:p"}),
    )
    .await
    .ok();

    // The agent's list: both, with no value.
    let mine = td.get_as("p/w", "/api/agent/secrets").await;
    clean(&mine.body);
    let rows = mine.ok().as_array().unwrap().clone();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0]["name"], "db");
    assert_eq!(rows[0]["level"], "read");
    assert_eq!(rows[0]["env"], "DB_KEY");
    assert_eq!(rows[1]["name"], "gh");
    assert_eq!(rows[1]["level"], "use");
    assert_eq!(rows[1]["fields"]["host"], "git.example.org");
    // The operator has no agent list.
    td.get("/api/agent/secrets").await.err(403, "forbidden");

    // A get: the value for `read`, refused for `use` and for no grant.
    let r = td
        .post_as("p/w", "/api/agent/secrets/db/get", json!({}))
        .await;
    assert_eq!(r.ok()["value"], PROJECT_VALUE);
    assert_eq!(r.headers["cache-control"], "no-store");
    let r = td.post_empty_as("p/w", "/api/agent/secrets/db/get").await;
    assert_eq!(r.ok()["value"], PROJECT_VALUE);
    td.post_as("p/w", "/api/agent/secrets/gh/get", json!({}))
        .await
        .err(403, "forbidden");
    td.post_as("p/w", "/api/agent/secrets/openai/get", json!({}))
        .await
        .err(404, "not_found");
    td.post_as("q/x", "/api/agent/secrets/db/get", json!({}))
        .await
        .err(404, "not_found");
    td.post_as("p/w", "/api/agent/secrets/db/get", json!({"field": "nope"}))
        .await
        .err(422, "invalid");
    let audit = td.get("/api/audit?action=secret.get").await;
    let rows = audit.ok().as_array().unwrap().clone();
    assert_eq!(rows.len(), 2);
    clean(&audit.body);

    // A revoke applies at the next get.
    let r = td
        .req(
            hyper::Method::DELETE,
            "/api/secrets/p/db/grants?grantee=agent:p/w",
            None,
            Some("p/lead"),
        )
        .await;
    assert_eq!(r.status.as_u16(), 204, "{:?}", r.body);
    td.post_as("p/w", "/api/agent/secrets/db/get", json!({}))
        .await
        .err(404, "not_found");
    td.req(
        hyper::Method::DELETE,
        "/api/secrets/p/db/grants?grantee=agent:p/w",
        None,
        None,
    )
    .await
    .err(404, "not_found");
    assert_eq!(
        td.get("/api/audit?action=secret.revoke")
            .await
            .ok()
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn the_credential_helper_answers_for_a_granted_host() {
    let td = setup().await;
    let ask = json!({"protocol": "https", "host": "git.example.org"});
    // No grant: nothing.
    td.post_as("p/w", "/api/repos/credential", ask.clone())
        .await
        .err(404, "not_found");
    // A `use` grant to the project: the helper answers, with a path too.
    td.post(
        "/api/secrets/global/gh/grants",
        json!({"grantee": "project:p", "level": "use"}),
    )
    .await
    .ok();
    let r = td
        .post_as("p/w", "/api/repos/credential", ask.clone())
        .await;
    assert_eq!(r.ok()["password"], TOKEN_VALUE);
    assert_eq!(r.ok()["username"], "x-access-token");
    let mut with_path = ask.clone();
    with_path["path"] = "team/repo.git".into();
    let r = td.post_as("p/w", "/api/repos/credential", with_path).await;
    assert_eq!(r.ok()["password"], TOKEN_VALUE);
    // Another host, another project, plain http: nothing.
    td.post_as(
        "p/w",
        "/api/repos/credential",
        json!({"protocol": "https", "host": "other.example.org"}),
    )
    .await
    .err(404, "not_found");
    td.post_as("q/x", "/api/repos/credential", ask.clone())
        .await
        .err(404, "not_found");
    td.post_as(
        "p/w",
        "/api/repos/credential",
        json!({"protocol": "http", "host": "git.example.org"}),
    )
    .await
    .err(404, "not_found");
    let audit = td.get("/api/audit?action=secret.use").await;
    let rows = audit.ok().as_array().unwrap().clone();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["target"], "global/gh");
    clean(&audit.body);
}

#[tokio::test]
async fn removing_an_agent_removes_its_grants() {
    let td = setup().await;
    td.post(
        "/api/secrets/global/openai/grants",
        json!({"grantee": "agent:p/w"}),
    )
    .await
    .ok();
    let r = td.delete("/api/agents/p%2Fw?confirm=true").await;
    assert_eq!(r.status.as_u16(), 204, "{:?}", r.body);
    td.agent("p", "w", json!({})).await;
    // The new agent with the same name has no grant.
    let mine = td.get_as("p/w", "/api/agent/secrets").await;
    assert_eq!(mine.ok(), &json!([]));
    let grants = td.get("/api/secrets/global/openai/grants").await;
    assert_eq!(grants.ok(), &json!([]));
}

/// Secret manager review S1 and S6: `terrarium exec-env` gets the env vars
/// of the `read` grants from `POST /api/agent/secrets/env` inside the
/// sandbox. The grant API refuses a name outside the name rule (`422`);
/// the audit row holds the names only; a revoke applies at the next call.
#[tokio::test]
async fn the_launch_env_comes_from_the_daemon_and_follows_the_name_rule() {
    let td = setup().await;
    for bad in [
        "NODE_OPTIONS",
        "HTTPS_PROXY",
        "OPENAI_API_KEY",
        "GIT_DIR",
        "db_key",
    ] {
        td.post(
            "/api/secrets/p/db/grants",
            json!({"grantee": "agent:p/w", "level": "read", "env": bad}),
        )
        .await
        .err(422, "invalid");
    }
    // A generic secret whose final names break the rule: `LD_PRELOAD`.
    td.put(
        "/api/secrets/p/pre",
        json!({"kind": "generic", "fields": {"preload": "/x.so", "path": "/y"},
               "secret_fields": ["preload", "path"]}),
    )
    .await
    .ok();
    td.post(
        "/api/secrets/p/pre/grants",
        json!({"grantee": "agent:p/w", "level": "read", "env": "LD"}),
    )
    .await
    .err(422, "invalid");
    td.post(
        "/api/secrets/p/db/grants",
        json!({"grantee": "agent:p/w", "level": "read", "env": "DB_KEY"}),
    )
    .await
    .ok();
    td.post(
        "/api/secrets/global/gh/grants",
        json!({"grantee": "project:p", "level": "read"}),
    )
    .await
    .ok();
    let r = td.post_empty_as("p/w", "/api/agent/secrets/env").await;
    assert_eq!(
        r.ok(),
        &json!({"vars": {"DB_KEY": PROJECT_VALUE, "GH_TOKEN": TOKEN_VALUE}, "credentials": {}})
    );
    assert_eq!(r.headers["cache-control"], "no-store");
    // Another project's agent gets none of them; the operator gets 403.
    let r = td.post_empty_as("q/x", "/api/agent/secrets/env").await;
    assert_eq!(r.ok(), &json!({"vars": {}, "credentials": {}}));
    td.post_empty("/api/agent/secrets/env")
        .await
        .err(403, "forbidden");
    let audit = td.get("/api/audit?action=secret.env").await;
    let rows = audit.ok().as_array().unwrap().clone();
    assert_eq!(rows.len(), 2, "{rows:?}");
    clean(&audit.body);
    assert!(audit.body.to_string().contains("DB_KEY"));
    // A revoke applies at the next call.
    let r = td
        .req(
            hyper::Method::DELETE,
            "/api/secrets/p/db/grants?grantee=agent:p/w",
            None,
            None,
        )
        .await;
    assert_eq!(r.status.as_u16(), 204);
    let r = td.post_empty_as("p/w", "/api/agent/secrets/env").await;
    assert_eq!(
        r.ok(),
        &json!({"vars": {"GH_TOKEN": TOKEN_VALUE}, "credentials": {}})
    );
}

/// PLAN §16b.4: the stored API key of the agent's subscription reaches
/// its harness through `POST /api/agent/secrets/env` (`credentials`), not
/// through the launch spec. Only the agent of the subscription gets it,
/// from its subscription at the time of the call; a grant of the same
/// name loses; the audit row holds the names only; `secret get` cannot
/// read the key.
#[tokio::test]
async fn the_subscription_key_comes_with_the_launch_env() {
    const KEY: &str = "sk-ant-SUBSCRIPTION-key-1";
    const ZAI: &str = "zai-SUBSCRIPTION-key-2";
    let td = setup().await;
    td.post(
        "/api/subscriptions",
        json!({"name": "keyed", "provider": "anthropic", "harnesses": ["claude"]}),
    )
    .await
    .ok();
    td.post(
        "/api/subscriptions/keyed/api-key",
        json!({"provider": "anthropic", "key": KEY}),
    )
    .await
    .ok();
    td.post(
        "/api/subscriptions",
        json!({"name": "glm", "provider": "zai", "plan": "coding", "harnesses": ["claude", "pi"]}),
    )
    .await
    .ok();
    td.post(
        "/api/subscriptions/glm/api-key",
        json!({"provider": "zai", "key": ZAI}),
    )
    .await
    .ok();
    let k = td.agent("p", "k", json!({"subscription": "keyed"})).await;
    td.agent("p", "z", json!({"harness": "pi", "subscription": "glm"}))
        .await;
    // A grant next to the key: both come, each in its own map.
    td.put(
        "/api/secrets/p/other",
        json!({"kind": "api_key", "fields": {"value": PROJECT_VALUE}}),
    )
    .await
    .ok();
    td.post(
        "/api/secrets/p/other/grants",
        json!({"grantee": "agent:p/k", "level": "read", "env": "MY_KEY"}),
    )
    .await
    .ok();

    let r = td.post_empty_as("p/k", "/api/agent/secrets/env").await;
    assert_eq!(r.ok()["credentials"], json!({"ANTHROPIC_API_KEY": KEY}));
    assert_eq!(r.ok()["vars"], json!({"MY_KEY": PROJECT_VALUE}));
    assert_eq!(r.headers["cache-control"], "no-store");
    let r = td.post_empty_as("p/z", "/api/agent/secrets/env").await;
    let c = &r.ok()["credentials"];
    assert_eq!(c["ZAI_API_KEY"], ZAI);
    assert_eq!(c["ANTHROPIC_AUTH_TOKEN"], ZAI);
    assert_eq!(c["ANTHROPIC_BASE_URL"], "https://api.z.ai/api/anthropic");
    // Another agent gets no key.
    for other in ["p/w", "q/x"] {
        let r = td.post_empty_as(other, "/api/agent/secrets/env").await;
        assert_eq!(r.ok()["credentials"], json!({}), "{other}");
        let text = r.body.to_string();
        assert!(!text.contains(KEY) && !text.contains(ZAI), "{text}");
    }
    // The key is not a secret of the store: `secret get` cannot read it.
    let mine = td.get_as("p/k", "/api/agent/secrets").await;
    assert!(!mine.body.to_string().contains(KEY), "{:?}", mine.body);
    let r = td
        .post_empty_as("p/k", "/api/agent/secrets/keyed/get")
        .await;
    assert!(r.status.as_u16() >= 400, "{:?}", r.body);
    assert!(!r.body.to_string().contains(KEY));
    // The audit row names the vars, with no value.
    let audit = td.get("/api/audit?action=secret.env").await;
    let text = audit.body.to_string();
    assert!(text.contains("ANTHROPIC_API_KEY"), "{text}");
    assert!(!text.contains(KEY) && !text.contains(ZAI), "{text}");
    // A key change applies at the next call (the next start).
    td.post(
        "/api/subscriptions/keyed/api-key",
        json!({"provider": "anthropic", "key": "sk-ant-NEW"}),
    )
    .await
    .ok();
    let r = td.post_empty_as("p/k", "/api/agent/secrets/env").await;
    assert_eq!(
        r.ok()["credentials"],
        json!({"ANTHROPIC_API_KEY": "sk-ant-NEW"})
    );
    // An agent moved off the subscription gets no key.
    td.patch(
        &format!("/api/agents/{}", k["id"].as_str().unwrap()),
        json!({"subscription": null}),
    )
    .await
    .ok();
    let r = td.post_empty_as("p/k", "/api/agent/secrets/env").await;
    assert_eq!(r.ok()["credentials"], json!({}));
}
