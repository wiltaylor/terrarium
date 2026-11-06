//! The rights that a spawn gives an ephemeral agent (TER-65,
//! `terrariumd::spawn_rights`): targets from the owner's policy or lease,
//! secret grants up to the owner's level, and the owner's account pool.
//! Each over-limit ask is refused with the right named, and the leases
//! and grants end with the ephemeral agent.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::TestDaemon;
use serde_json::{Value, json};
use terrarium_core::HarnessId;
use terrarium_core::config::{CompactMaxAge, IdleStop};

const KEY: &str = "sk-spawn-rights-7Qm2Lx9Vb4Tz";
const TOKEN: &str = "ghp_spawnRights4Kw8nPdT2xLm";

fn state(td: &TestDaemon) -> terrariumd::api::Shared {
    td.daemon.as_ref().unwrap().state.clone()
}

fn fast(td: &TestDaemon) {
    let s = state(td);
    s.lifecycle.set_tuning(terrariumd::lifecycle::Tuning {
        idle_stop: IdleStop::Off,
        tick: Duration::from_millis(20),
        compact_on_idle: false,
        compact_max_age: CompactMaxAge::Off,
        compact_wait: Duration::from_secs(5),
    });
    s.lifecycle.spawns.set_tuning(terrariumd::spawn::Tuning {
        done_delay: Duration::from_millis(50),
    });
}

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

/// A project `p` with the template `worker`, a lead, a worker `w` that
/// may spawn `worker` and uses the global target `box`, the project
/// targets `pbox` and `xbox`, the global target `other`, and the secrets
/// `global/openai` (`w` reads it, env `OPENAI_KEY`), `global/gh` (`w` uses
/// it), and `p/db` (no grant).
async fn setup() -> (TestDaemon, String, String) {
    let td = TestDaemon::start().await;
    fast(&td);
    td.project("p").await;
    td.post(
        "/api/templates",
        json!({"name": "worker", "project": "p", "spec": {"harness": "claude"}}),
    )
    .await
    .ok();
    for t in [
        json!({"name": "box", "kind": "ssh", "address": "u@h"}),
        json!({"name": "other", "kind": "ssh", "address": "u@h"}),
        json!({"name": "pbox", "kind": "ssh", "project": "p", "address": "u@h"}),
        json!({"name": "xbox", "kind": "ssh", "project": "p", "address": "u@h"}),
    ] {
        td.post("/api/targets", t).await.ok();
    }
    let lead = id(&td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await);
    let w = id(&td
        .agent(
            "p",
            "w",
            json!({"policy": {"targets": ["box"],
                              "spawn": {"templates": ["worker"], "max_concurrent": 4}}}),
        )
        .await);
    td.put(
        "/api/secrets/global/openai",
        json!({"kind": "api_key", "fields": {"value": KEY}}),
    )
    .await
    .ok();
    td.put(
        "/api/secrets/global/gh",
        json!({"kind": "github_token", "fields": {"value": TOKEN, "host": "git.example.org"}}),
    )
    .await
    .ok();
    td.put(
        "/api/secrets/p/db",
        json!({"kind": "api_key", "fields": {"value": KEY}}),
    )
    .await
    .ok();
    td.post(
        "/api/secrets/global/openai/grants",
        json!({"grantee": "agent:p/w", "level": "read", "env": "OPENAI_KEY"}),
    )
    .await
    .ok();
    td.post(
        "/api/secrets/global/gh/grants",
        json!({"grantee": "agent:p/w", "level": "use"}),
    )
    .await
    .ok();
    (td, lead, w)
}

async fn spawn_as(td: &TestDaemon, by: &str, owner: &str, extra: Value) -> common::Resp {
    let mut body = json!({"template": "worker", "task": "do it"});
    for (k, v) in extra.as_object().unwrap() {
        body[k] = v.clone();
    }
    td.post_as(by, &format!("/api/agents/{owner}/spawn"), body)
        .await
}

fn message(r: &common::Resp) -> String {
    r.body["error"]["message"].as_str().unwrap_or("").to_owned()
}

async fn grants_of(td: &TestDaemon, secret: &str) -> Vec<Value> {
    td.get(&format!("/api/secrets/{secret}/grants"))
        .await
        .ok()
        .as_array()
        .unwrap()
        .clone()
}

async fn leases_of(td: &TestDaemon, agent: &str) -> Vec<Value> {
    td.get(&format!("/api/agents/{agent}/leases"))
        .await
        .ok()
        .as_array()
        .unwrap()
        .clone()
}

async fn cancel(td: &TestDaemon, child: &str) {
    let r = td
        .post(&format!("/api/agents/{child}/cancel"), json!({}))
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
}

/// A target in the owner's policy goes into the child's policy, also a
/// global one that the lead could not add by `agent set`. A target that
/// the owner may not use is 403 with the target named, also when the lead
/// spawns for the owner.
#[tokio::test]
async fn targets_come_only_from_the_owner_policy() {
    let (td, lead, w) = setup().await;
    let r = spawn_as(&td, &w, &w, json!({"targets": ["box"]})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["agent"]["policy"]["targets"], json!(["box"]));

    let r = spawn_as(&td, &w, &w, json!({"targets": ["other"]})).await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert!(message(&r).contains("target `other`"), "{}", r.body);

    // The lead spawns for `w`: the rights are `w`'s, not the lead's.
    let r = spawn_as(&td, &lead, &w, json!({"targets": ["box"]})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let r = spawn_as(&td, &lead, &w, json!({"targets": ["pbox"]})).await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert!(message(&r).contains("target `pbox`"), "{}", r.body);

    // An unknown target is 404; a bad list is 422.
    let r = spawn_as(&td, &w, &w, json!({"targets": ["nope"]})).await;
    assert_eq!(r.status, 404, "{}", r.body);
    let r = spawn_as(&td, &w, &w, json!({"targets": [" "]})).await;
    assert_eq!(r.status, 422, "{}", r.body);

    // The audit row names the owner and the targets.
    let audit = td.get("/api/audit?action=agent.spawn").await.body.to_string();
    assert!(audit.contains(&w), "{audit}");
    assert!(audit.contains("\"targets\":[\"box\"]"), "{audit}");
}

/// A target that the owner holds by a lease gives the child a lease that
/// ends no later than the owner's lease; the close revokes it. An
/// exclusive lease of the owner is 409.
#[tokio::test]
async fn a_leased_target_gives_a_shorter_lease_that_ends_at_the_close() {
    let (td, _lead, w) = setup().await;
    let until = "2099-01-01T00:00:00.000Z";
    td.post(
        "/api/targets/pbox/leases",
        json!({"agent": "w", "until": until}),
    )
    .await
    .ok();
    let r = spawn_as(&td, &w, &w, json!({"targets": ["pbox"]})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let child = id(&r.body);
    assert!(
        r.body["agent"]["policy"]["targets"]
            .as_array()
            .is_none_or(|a| a.is_empty()),
        "{}",
        r.body
    );
    let held = leases_of(&td, &child).await;
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0]["target"], "pbox");
    assert_eq!(held[0]["state"], "active");
    assert_eq!(held[0]["exclusive"], false);
    // The lifetime (2 h) ends before the owner's lease.
    let ends = held[0]["expires_at"].as_str().unwrap().to_owned();
    assert!(ends.as_str() < until, "{ends}");
    assert_eq!(ends, r.body["deadline_at"].as_str().unwrap());

    cancel(&td, &child).await;
    let after = td
        .get("/api/targets/pbox/leases?all=true")
        .await
        .ok()
        .as_array()
        .unwrap()
        .clone();
    let mine: Vec<&Value> = after.iter().filter(|l| l["agent"] == child).collect();
    assert_eq!(mine.len(), 1, "{after:?}");
    assert_eq!(mine[0]["state"], "revoked");

    // An exclusive lease has room for one holder.
    td.post(
        "/api/targets/xbox/leases",
        json!({"agent": "w", "exclusive": true}),
    )
    .await
    .ok();
    let r = spawn_as(&td, &w, &w, json!({"targets": ["xbox"]})).await;
    assert_eq!(r.status, 409, "{}", r.body);
    assert!(message(&r).contains("exclusive"), "{}", r.body);
}

/// A grant needs the owner's grant at that level or higher. No level: the
/// kind's default, at most the owner's. A read grant keeps the owner's env
/// name. The grants end with the child.
#[tokio::test]
async fn grants_stay_within_the_owner_level_and_end_with_the_child() {
    let (td, _lead, w) = setup().await;
    let r = spawn_as(&td, &w, &w, json!({"grants": ["openai", "global/gh"]})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let child = id(&r.body);
    let key = format!("agent:{child}");
    let g = grants_of(&td, "global/openai").await;
    let mine: Vec<&Value> = g.iter().filter(|x| x["grantee"] == key).collect();
    assert_eq!(mine.len(), 1, "{g:?}");
    assert_eq!(mine[0]["level"], "read");
    assert_eq!(mine[0]["env"], "OPENAI_KEY");
    let g = grants_of(&td, "global/gh").await;
    let mine: Vec<&Value> = g.iter().filter(|x| x["grantee"] == key).collect();
    assert_eq!(mine[0]["level"], "use");
    assert!(mine[0].get("env").is_none(), "{g:?}");
    // The audit names the owner, with no value.
    let audit = td.get("/api/audit?action=secret.grant").await.body.to_string();
    assert!(audit.contains(&format!("\"spawn_owner\":\"{w}\"")), "{audit}");
    assert!(!audit.contains(KEY) && !audit.contains(TOKEN));

    // A use grant does not give read; no grant gives nothing; the
    // project secret of another scope is out of reach.
    let r = spawn_as(&td, &w, &w, json!({"grants": ["gh:read"]})).await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert!(message(&r).contains("level `read`"), "{}", r.body);
    let r = spawn_as(&td, &w, &w, json!({"grants": ["db"]})).await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert!(message(&r).contains("secret `db`"), "{}", r.body);
    let r = spawn_as(&td, &w, &w, json!({"grants": ["q/db"]})).await;
    assert_eq!(r.status, 403, "{}", r.body);
    let r = spawn_as(&td, &w, &w, json!({"grants": ["openai:write"]})).await;
    assert_eq!(r.status, 422, "{}", r.body);
    // A refused spawn makes no grant: `w` and the first child only.
    assert_eq!(grants_of(&td, "global/gh").await.len(), 2);

    cancel(&td, &child).await;
    let g = grants_of(&td, "global/openai").await;
    assert!(g.iter().all(|x| x["grantee"] != key), "{g:?}");
    let g = grants_of(&td, "global/gh").await;
    assert!(g.iter().all(|x| x["grantee"] != key), "{g:?}");
}

/// The operator as owner holds every right: any target and secret that
/// the project can reach.
#[tokio::test]
async fn the_operator_owner_gives_any_reachable_right() {
    let (td, _lead, _w) = setup().await;
    let r = td
        .post(
            "/api/agents/operator/spawn",
            json!({"template": "worker", "task": "t", "project": "p",
                   "targets": ["other", "pbox"], "grants": ["db"]}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["agent"]["policy"]["targets"], json!(["other", "pbox"]));
    let child = id(&r.body);
    let g = grants_of(&td, "p/db").await;
    assert!(
        g.iter().any(|x| x["grantee"] == format!("agent:{child}")),
        "{g:?}"
    );
}

/// A template with no account takes the owner's account and pool; a
/// template with its own account keeps it.
#[tokio::test]
async fn the_child_takes_the_owner_pool() {
    let (td, _lead, _w) = setup().await;
    let s = state(&td);
    for n in ["max-a", "max-b"] {
        td.post(
            "/api/subscriptions",
            json!({"name": n, "provider": "anthropic", "harnesses": ["claude"]}),
        )
        .await
        .ok();
        let login = r#"{"claudeAiOauth": {"accessToken": "t", "refreshToken": "r", "expiresAt": 4102444800000}}"#;
        terrariumd::accounts::write_credential(&s, n, HarnessId::Claude, login.as_bytes())
            .unwrap();
    }
    let o = id(&td
        .agent(
            "p",
            "pooled",
            json!({"subscription": "max-a", "subscription_pool": ["max-b"],
                   "policy": {"spawn": {"templates": ["worker", "fixed"]}}}),
        )
        .await);
    let r = spawn_as(&td, &o, &o, json!({})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["agent"]["subscription"], "max-a");
    assert_eq!(r.body["agent"]["subscription_pool"], json!(["max-a", "max-b"]));

    td.post(
        "/api/templates",
        json!({"name": "fixed", "project": "p",
               "spec": {"harness": "claude", "subscription": "max-b"}}),
    )
    .await
    .ok();
    let r = spawn_as(&td, &o, &o, json!({"template": "fixed"})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["agent"]["subscription"], "max-b");
    assert!(r.body["agent"]["subscription_pool"].is_null(), "{}", r.body);
}
