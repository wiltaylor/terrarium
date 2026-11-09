//! The rights that a spawn gives an ephemeral agent (TER-65,
//! `terrariumd::spawn_rights`): targets from the owner's policy or lease,
//! secret grants up to the owner's level, and the owner's account pool.
//! Each over-limit ask is refused with the right named, and the leases
//! and grants end with the ephemeral agent.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

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
/// targets `p-pbox` and `p-xbox`, the global target `other`, and the secrets
/// `global/openai` (`w` reads it, env `SPAWN_RIGHTS_DB_PASS`), `global/gh` (`w` uses
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
        json!({"name": "p-pbox", "kind": "ssh", "project": "p", "address": "u@h"}),
        json!({"name": "p-xbox", "kind": "ssh", "project": "p", "address": "u@h"}),
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
        json!({"grantee": "agent:p/w", "level": "read", "env": "SPAWN_RIGHTS_DB_PASS"}),
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

#[tokio::test]
async fn like_copies_agent_rights_and_skills() {
    let (td, _, w) = setup().await;
    let r = spawn_as(&td, &w, &w, json!({"like": "w"})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["agent"]["policy"]["targets"], json!(["box"]));
    assert_eq!(r.body["skipped"], json!([]));
    let child = r.body["id"].as_str().unwrap();
    let grants = grants_of(&td, "global/openai").await;
    assert!(
        grants
            .iter()
            .any(|g| g["grantee"] == format!("agent:{child}")
                && g["level"] == "read"
                && g["env"] == "SPAWN_RIGHTS_DB_PASS")
    );
    let source_response = td.get(&format!("/api/agents/{w}")).await;
    let source = source_response.ok();
    for field in [
        "skills",
        "skills_off",
        "default_skills",
        "subscription_pool",
    ] {
        assert_eq!(r.body["agent"][field], source[field], "{field}");
    }
}

#[tokio::test]
async fn like_never_copies_authority_past_owner() {
    let (td, lead, w) = setup().await;
    td.patch(
        &format!("/api/agents/{lead}"),
        json!({"policy": {
            "spawn": {"templates": ["worker"], "max_concurrent": 4}
        }}),
    )
    .await
    .ok();
    // A lead with no target or secret rights cannot obtain those of w.
    let r = spawn_as(&td, &lead, &lead, json!({"like": "w"})).await;
    assert_eq!(r.status, 403, "{}", r.body);
    let r = spawn_as(
        &td,
        &lead,
        &lead,
        json!({"like": "w", "like_skip_missing": true}),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert!(
        r.body["skipped"]
            .as_array()
            .unwrap()
            .contains(&json!("target:box"))
    );
    assert!(
        r.body["agent"]["policy"]["targets"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let child = r.body["id"].as_str().unwrap();
    assert!(
        !grants_of(&td, "global/openai")
            .await
            .iter()
            .any(|g| g["grantee"] == format!("agent:{child}"))
    );
    // A lead spawning for w still uses w's rights: the child of that
    // spawn gets w's target and grant (TER-391 review: more than the 201).
    let r = spawn_as(&td, &lead, &w, json!({"like": "w"})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["agent"]["policy"]["targets"], json!(["box"]));
    let child = id(&r.body);
    let g = grants_to(&td, "global/openai", &child).await;
    assert_eq!(g.len(), 1, "{g:?}");
    assert_eq!(g[0]["level"], "read");
}

#[tokio::test]
async fn like_rejects_cross_project_sources() {
    let (td, _, w) = setup().await;
    td.project("q").await;
    td.agent("q", "foreign", json!({})).await;
    let r = spawn_as(
        &td,
        &w,
        &w,
        json!({"like": "q/foreign", "like_skip_missing": true}),
    )
    .await;
    assert_eq!(r.status, 403, "{}", r.body);
}

/// TER-391 (TER-89 review): a copy from another agent, a 403 that names
/// the refused right, and `skipped` entries that name a grant only when
/// the owner may already read the secret's name. The others are counted:
/// the source's grant list is not exposed (the caller may not read it).
#[tokio::test]
async fn like_skips_name_only_grants_the_owner_may_read() {
    let (td, _, _w) = setup().await;
    // `low` may use `box` and holds `global/openai` at `use`: it can read
    // the names of `box` and `global/openai`, not of `global/gh`.
    let low = id(&td
        .agent(
            "p",
            "low",
            json!({"policy": {"targets": ["box"],
                              "spawn": {"templates": ["worker"], "max_concurrent": 4}}}),
        )
        .await);
    td.post(
        "/api/secrets/global/openai/grants",
        json!({"grantee": "agent:p/low", "level": "use"}),
    )
    .await
    .ok();

    // No skip: a refused copied grant is named only when the owner may
    // read its name (`openai`), never `gh` (TER-391 round 2).
    let r = spawn_as(&td, &low, &low, json!({"like": "w"})).await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert!(message(&r).contains("secret `global/openai`"), "{}", r.body);
    assert!(!r.body.to_string().contains("global/gh"), "{}", r.body);

    // No skip, and every refused grant is hidden: a 403 with the count
    // and no secret name.
    let mid = id(&td
        .agent(
            "p",
            "mid",
            json!({"policy": {"targets": ["box"],
                              "spawn": {"templates": ["worker"], "max_concurrent": 4}}}),
        )
        .await);
    let r = spawn_as(&td, &mid, &mid, json!({"like": "w"})).await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert!(message(&r).contains("2 copied secret grant(s)"), "{}", r.body);
    assert!(message(&r).contains("like_skip_missing"), "{}", r.body);
    let body = r.body.to_string();
    assert!(!body.contains("openai") && !body.contains("global/gh"), "{body}");

    // With the skip: a named entry for the secret whose name the owner
    // may read, one count for `gh`, whose name it may not.
    let r = spawn_as(
        &td,
        &low,
        &low,
        json!({"like": "w", "like_skip_missing": true}),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(
        r.body["skipped"],
        json!([
            "grant:global/openai:read",
            "grant:1 hidden: the owner may not read their names"
        ])
    );
    let child = id(&r.body);
    assert_eq!(r.body["agent"]["policy"]["targets"], json!(["box"]));
    assert!(grants_to(&td, "global/openai", &child).await.is_empty());
    assert!(grants_to(&td, "global/gh", &child).await.is_empty());

    // An owner with no right at all learns no name, only the counts.
    let weak = id(&td
        .agent(
            "p",
            "weak",
            json!({"policy": {"spawn": {"templates": ["worker"]}}}),
        )
        .await);
    let r = spawn_as(
        &td,
        &weak,
        &weak,
        json!({"like": "w", "like_skip_missing": true}),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(
        r.body["skipped"],
        json!([
            "target:box",
            "grant:2 hidden: the owner may not read their names"
        ])
    );
    let body = r.body.to_string();
    assert!(!body.contains("openai") && !body.contains("/gh"), "{body}");
    let child = id(&r.body);
    assert!(grants_to(&td, "global/openai", &child).await.is_empty());
    assert!(grants_to(&td, "global/gh", &child).await.is_empty());
}

/// TER-391 (TER-89 review): a body grant replaces the copied grant of
/// the same secret, also at a lower level.
#[tokio::test]
async fn a_body_grant_replaces_the_copied_grant() {
    let (td, _, w) = setup().await;
    let r = spawn_as(&td, &w, &w, json!({"like": "w", "grants": ["openai:use"]})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let child = id(&r.body);
    let g = grants_to(&td, "global/openai", &child).await;
    assert_eq!(g.len(), 1, "{g:?}");
    assert_eq!(g[0]["level"], "use");
    assert!(g[0].get("env").is_none(), "{g:?}");
    // The other copied grant still comes.
    let g = grants_to(&td, "global/gh", &child).await;
    assert_eq!(g.len(), 1, "{g:?}");
    assert_eq!(g[0]["level"], "use");
}

/// TER-391 (TER-89 review): a copied target that the body also names is
/// granted once, also when the owner holds it by a lease.
#[tokio::test]
async fn a_copied_target_is_granted_once() {
    let (td, _, w) = setup().await;
    let _src = id(&td
        .agent("p", "src", json!({"policy": {"targets": ["p-pbox"]}}))
        .await);
    td.post(
        "/api/targets/p-pbox/leases",
        json!({"agent": "w", "until": "2099-01-01T00:00:00.000Z"}),
    )
    .await
    .ok();
    let r = spawn_as(&td, &w, &w, json!({"targets": ["p-pbox"], "like": "src"})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let child = id(&r.body);
    let held: Vec<Value> = leases_of(&td, &child)
        .await
        .into_iter()
        .filter(|l| l["target"] == "p-pbox")
        .collect();
    assert_eq!(held.len(), 1, "{held:?}");
    // A policy target twice is still one entry in the child's policy.
    let r = spawn_as(&td, &w, &w, json!({"targets": ["box"], "like": "w"})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["agent"]["policy"]["targets"], json!(["box"]));
}

/// TER-391 (TER-89 review): the source's harness must match the chosen
/// template, and the error names it, also when it is the alternative
/// picked for headroom. An unknown source is 404.
#[tokio::test]
async fn like_harness_and_source_errors_name_the_cause() {
    let (td, _, w) = setup().await;
    td.post(
        "/api/subscriptions",
        json!({"name": "hsub", "provider": "anthropic", "harnesses": ["claude"]}),
    )
    .await
    .ok();
    for (name, spec) in [
        ("pitemp", json!({"harness": "pi"})),
        (
            "altcase",
            json!({"harness": "claude", "subscription": "hsub"}),
        ),
        ("altcase-alt", json!({"harness": "pi"})),
    ] {
        td.post(
            "/api/templates",
            json!({"name": name, "project": "p", "spec": spec}),
        )
        .await
        .ok();
    }
    td.patch(
        &format!("/api/agents/{w}"),
        json!({"policy": {"targets": ["box"],
                          "spawn": {"templates": ["worker", "pitemp", "altcase", "altcase-alt"],
                                    "max_concurrent": 4}}}),
    )
    .await
    .ok();
    let r = spawn_as(&td, &w, &w, json!({"template": "pitemp", "like": "w"})).await;
    assert_eq!(r.status, 422, "{}", r.body);
    assert!(
        message(&r).contains("same harness as the template `pitemp`"),
        "{}",
        r.body
    );
    // `altcase` asks for `hsub`; the like source `hs` runs `hsub` under
    // an active usage limit, so the first template has no headroom and
    // the alternative is tried: its harness differs, and the error names
    // `altcase-alt`, not just `altcase`.
    let login = r#"{"claudeAiOauth": {"accessToken": "t", "refreshToken": "r", "expiresAt": 4102444800000}}"#;
    terrariumd::accounts::write_credential(
        &state(&td).state_dir,
        "hsub",
        HarnessId::Claude,
        login.as_bytes(),
    )
    .unwrap();
    let _hs = id(&td.agent("p", "hs", json!({"subscription": "hsub"})).await);
    state(&td).usage.limits.mark("hsub", None);
    let r = spawn_as(&td, &w, &w, json!({"template": "altcase", "like": "hs"})).await;
    assert_eq!(r.status, 422, "{}", r.body);
    assert!(
        message(&r).contains("same harness as the template `altcase-alt`"),
        "{}",
        r.body
    );
    let r = spawn_as(&td, &w, &w, json!({"like": "nope"})).await;
    assert_eq!(r.status, 404, "{}", r.body);
    assert!(message(&r).contains("no agent `nope`"), "{}", r.body);
}

/// TER-391 (TER-89 review): `--like` copies the source's subscription
/// and pool over the template's and the owner's, one rule in the
/// selection and the write.
#[tokio::test]
async fn like_replaces_the_subscription_and_pool() {
    let (td, _, _w) = setup().await;
    let s = state(&td).state_dir.clone();
    for n in ["like-a", "like-b"] {
        td.post(
            "/api/subscriptions",
            json!({"name": n, "provider": "anthropic", "harnesses": ["claude"]}),
        )
        .await
        .ok();
        let login = r#"{"claudeAiOauth": {"accessToken": "t", "refreshToken": "r", "expiresAt": 4102444800000}}"#;
        terrariumd::accounts::write_credential(&s, n, HarnessId::Claude, login.as_bytes()).unwrap();
    }
    td.post(
        "/api/templates",
        json!({"name": "fixed", "project": "p",
               "spec": {"harness": "claude", "subscription": "like-b"}}),
    )
    .await
    .ok();
    let pooled = id(&td
        .agent(
            "p",
            "pooled",
            json!({"subscription": "like-a", "subscription_pool": ["like-b"],
                   "policy": {"spawn": {"templates": ["worker", "fixed"], "max_concurrent": 8}}}),
        )
        .await);
    let _bare = id(&td.agent("p", "bare", json!({})).await);
    let _rich = id(&td
        .agent(
            "p",
            "rich",
            json!({"subscription": "like-a", "subscription_pool": ["like-b"]}),
        )
        .await);

    // A source with none: the child gets none, not the owner's, not the
    // template's, and the inherited pool is cleared.
    let r = spawn_as(&td, &pooled, &pooled, json!({"like": "bare"})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert!(r.body["agent"]["subscription"].is_null(), "{}", r.body);
    assert!(r.body["agent"]["subscription_pool"].is_null(), "{}", r.body);
    let r = spawn_as(
        &td,
        &pooled,
        &pooled,
        json!({"template": "fixed", "like": "bare"}),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert!(r.body["agent"]["subscription"].is_null(), "{}", r.body);

    // A source with its own: the child gets the source's pick and pool.
    let r = spawn_as(&td, &pooled, &pooled, json!({"like": "rich"})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["agent"]["subscription"], "like-a");
    assert_eq!(
        r.body["agent"]["subscription_pool"],
        json!(["like-a", "like-b"])
    );
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
    let r = spawn_as(&td, &lead, &w, json!({"targets": ["p-pbox"]})).await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert!(message(&r).contains("target `p-pbox`"), "{}", r.body);

    // An unknown target is 404; a bad list is 422.
    let r = spawn_as(&td, &w, &w, json!({"targets": ["nope"]})).await;
    assert_eq!(r.status, 404, "{}", r.body);
    let r = spawn_as(&td, &w, &w, json!({"targets": [" "]})).await;
    assert_eq!(r.status, 422, "{}", r.body);

    // The audit row names the owner and the targets.
    let audit = td
        .get("/api/audit?action=agent.spawn")
        .await
        .body
        .to_string();
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
        "/api/targets/p-pbox/leases",
        json!({"agent": "w", "until": until}),
    )
    .await
    .ok();
    let r = spawn_as(&td, &w, &w, json!({"targets": ["p-pbox"]})).await;
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
    assert_eq!(held[0]["target"], "p-pbox");
    assert_eq!(held[0]["state"], "active");
    assert_eq!(held[0]["exclusive"], false);
    // The lifetime (2 h) ends before the owner's lease.
    let ends = held[0]["expires_at"].as_str().unwrap().to_owned();
    assert!(ends.as_str() < until, "{ends}");
    assert_eq!(ends, r.body["deadline_at"].as_str().unwrap());

    cancel(&td, &child).await;
    let after = td
        .get("/api/targets/p-pbox/leases?all=true")
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
        "/api/targets/p-xbox/leases",
        json!({"agent": "w", "exclusive": true}),
    )
    .await
    .ok();
    let r = spawn_as(&td, &w, &w, json!({"targets": ["p-xbox"]})).await;
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
    assert_eq!(mine[0]["env"], "SPAWN_RIGHTS_DB_PASS");
    let g = grants_of(&td, "global/gh").await;
    let mine: Vec<&Value> = g.iter().filter(|x| x["grantee"] == key).collect();
    assert_eq!(mine[0]["level"], "use");
    assert!(mine[0].get("env").is_none(), "{g:?}");
    // The audit names the owner, with no value.
    let audit = td
        .get("/api/audit?action=secret.grant")
        .await
        .body
        .to_string();
    assert!(
        audit.contains(&format!("\"spawn_owner\":\"{w}\"")),
        "{audit}"
    );
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

/// TER-142: `:run` on a spawn. The owner check is `use` < `run` < `read`.
/// A `run` grant of the child never goes past the owner's targets: the
/// owner's run targets (all, or a subset), or for an owner with `read`
/// the targets that it names and may use.
#[tokio::test]
async fn run_grants_stay_within_the_owner_targets() {
    let (td, _lead, w) = setup().await;
    // `w` reads openai (no target limit) and holds db at `run` on box
    // and p-pbox.
    let r = td
        .post(
            "/api/secrets/p/db/grants",
            json!({"grantee": "agent:p/w", "level": "run", "targets": ["p-pbox", "box"]}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["targets"], json!(["box", "p-pbox"]));

    // `:run` with no targets: the owner's run targets.
    let r = spawn_as(&td, &w, &w, json!({"grants": ["db:run"]})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let child = id(&r.body);
    let g = grants_of(&td, "p/db").await;
    let mine: Vec<&Value> = g
        .iter()
        .filter(|x| x["grantee"] == format!("agent:{child}"))
        .collect();
    assert_eq!(mine.len(), 1, "{g:?}");
    assert_eq!(mine[0]["level"], "run");
    assert_eq!(mine[0]["targets"], json!(["box", "p-pbox"]));
    assert!(mine[0].get("env").is_none(), "{g:?}");
    // A subset is fine; a target past the owner's run targets is 403.
    let r = spawn_as(&td, &w, &w, json!({"grants": ["db:run=box"]})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let r = spawn_as(&td, &w, &w, json!({"grants": ["db:run=box,other"]})).await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert!(message(&r).contains("`other`"), "{}", r.body);
    // `run` does not give `read`.
    let r = spawn_as(&td, &w, &w, json!({"grants": ["db:read"]})).await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert!(message(&r).contains("level `read`"), "{}", r.body);
    // `use` does not give `run`.
    let r = spawn_as(&td, &w, &w, json!({"grants": ["gh:run=box"]})).await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert!(message(&r).contains("level `run`"), "{}", r.body);

    // An owner with `read` names the targets, each one it may use.
    let r = spawn_as(&td, &w, &w, json!({"grants": ["openai:run"]})).await;
    assert_eq!(r.status, 422, "{}", r.body);
    let r = spawn_as(&td, &w, &w, json!({"grants": ["openai:run=other"]})).await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert!(message(&r).contains("`other`"), "{}", r.body);
    let r = spawn_as(&td, &w, &w, json!({"grants": ["openai:run=box"]})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let child = id(&r.body);
    let g = grants_of(&td, "global/openai").await;
    let mine: Vec<&Value> = g
        .iter()
        .filter(|x| x["grantee"] == format!("agent:{child}"))
        .collect();
    assert_eq!(mine[0]["level"], "run");
    assert_eq!(mine[0]["targets"], json!(["box"]));
    // A `run` grant keeps no env var name.
    assert!(mine[0].get("env").is_none(), "{g:?}");

    // The operator as owner: any existing target that the project reaches.
    let r = td
        .post(
            "/api/agents/operator/spawn",
            json!({"template": "worker", "task": "t", "project": "p",
                   "grants": ["db:run=other"]}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let r = td
        .post(
            "/api/agents/operator/spawn",
            json!({"template": "worker", "task": "t", "project": "p",
                   "grants": ["db:run"]}),
        )
        .await;
    assert_eq!(r.status, 422, "{}", r.body);
    let r = td
        .post(
            "/api/agents/operator/spawn",
            json!({"template": "worker", "task": "t", "project": "p",
                   "grants": ["db:run=nope"]}),
        )
        .await;
    assert_eq!(r.status, 404, "{}", r.body);
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
                   "targets": ["other", "p-pbox"], "grants": ["db"]}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(
        r.body["agent"]["policy"]["targets"],
        json!(["other", "p-pbox"])
    );
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
    let s = state(&td).state_dir.clone();
    for n in ["max-a", "max-b"] {
        td.post(
            "/api/subscriptions",
            json!({"name": n, "provider": "anthropic", "harnesses": ["claude"]}),
        )
        .await
        .ok();
        let login = r#"{"claudeAiOauth": {"accessToken": "t", "refreshToken": "r", "expiresAt": 4102444800000}}"#;
        terrariumd::accounts::write_credential(&s, n, HarnessId::Claude, login.as_bytes()).unwrap();
    }
    let o = id(&td
        .agent(
            "p",
            "pooled",
            json!({"subscription": "max-a", "subscription_pool": ["max-b"],
                   "policy": {"spawn": {"templates": ["worker", "fixed"], "max_concurrent": 20}}}),
        )
        .await);
    let r = spawn_as(&td, &o, &o, json!({})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["agent"]["subscription"], "max-a");
    assert_eq!(
        r.body["agent"]["subscription_pool"],
        json!(["max-a", "max-b"])
    );

    // Provider refusals override absent (or cool) probe samples, also
    // for the pool inherited from the owner.
    let runtime = state(&td);
    runtime.usage.limits.mark("max-a", None);
    let r = spawn_as(&td, &o, &o, json!({})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["agent"]["subscription"], "max-b");
    runtime.usage.limits.mark("max-b", None);
    let r = spawn_as(&td, &o, &o, json!({})).await;
    assert_eq!(r.status, 409, "{}", r.body);
    assert!(message(&r).contains("max-a: active usage limit"));
    runtime.usage.limits.clear("max-b");
    runtime.usage.limits.mark(
        "max-a",
        Some(chrono::Utc::now() - chrono::Duration::seconds(1)),
    );
    let r = spawn_as(&td, &o, &o, json!({})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["agent"]["subscription"], "max-a");

    runtime
        .db
        .write("system", |t| {
            terrariumd::store::subscriptions::set_enabled(t, "max-a", false)?;
            Ok(())
        })
        .await
        .unwrap();
    let r = spawn_as(&td, &o, &o, json!({})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["agent"]["subscription"], "max-b");
    runtime
        .db
        .write("system", |t| {
            terrariumd::store::subscriptions::set_enabled(t, "max-a", true)?;
            terrariumd::store::subscriptions::set_login_status(t, "max-a", "expired")
        })
        .await
        .unwrap();
    let r = spawn_as(&td, &o, &o, json!({})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["agent"]["subscription"], "max-b");
    runtime
        .db
        .write("system", |t| {
            terrariumd::store::subscriptions::set_login_status(t, "max-a", "unknown")
        })
        .await
        .unwrap();
    terrariumd::accounts::remove(&runtime.state_dir, "max-a").unwrap();
    let r = spawn_as(&td, &o, &o, json!({})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["agent"]["subscription"], "max-b");

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

#[tokio::test]
async fn spawn_headroom_picks_pool_then_allowed_alternative() {
    let (td, _lead, _w) = setup().await;
    let s = state(&td);
    for n in ["pool-a", "pool-b", "pool-alt"] {
        td.post(
            "/api/subscriptions",
            json!({"name": n, "provider": "anthropic",
            "harnesses": ["claude"]}),
        )
        .await
        .ok();
        let login = r#"{"claudeAiOauth":{"accessToken":"test","refreshToken":"test","expiresAt":4102444800000}}"#;
        terrariumd::accounts::write_credential(
            &s.state_dir,
            n,
            HarnessId::Claude,
            login.as_bytes(),
        )
        .unwrap();
    }
    for (name, spec) in [
        (
            "headroom",
            json!({"harness": "claude", "subscription": "pool-a", "subscription_pool": ["pool-b"]}),
        ),
        (
            "headroom-alt",
            json!({"harness": "claude", "subscription": "pool-alt"}),
        ),
    ] {
        td.post(
            "/api/templates",
            json!({"name": name, "project": "p", "spec": spec}),
        )
        .await
        .ok();
    }
    let o = id(&td
        .agent(
            "p",
            "selector",
            json!({"policy": {"spawn": {
        "templates": ["headroom", "headroom-alt"], "max_concurrent": 10}}}),
        )
        .await);
    s.db.write("system", |t| {
        terrariumd::store::subscriptions::insert_sample(t, "pool-a", "5h", 79.9, None, &json!({}))?;
        terrariumd::store::subscriptions::insert_sample(t, "pool-b", "7d", 20.0, None, &json!({}))
    })
    .await
    .unwrap();
    let r = spawn_as(&td, &o, &o, json!({"template": "headroom"})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["agent"]["subscription"], "pool-b");
    s.db.write("system", |t| {
        terrariumd::store::subscriptions::insert_sample(t, "pool-a", "7d", 80.0, None, &json!({}))?;
        terrariumd::store::subscriptions::insert_sample(t, "pool-b", "5h", 90.0, None, &json!({}))
    })
    .await
    .unwrap();
    let r = spawn_as(&td, &o, &o, json!({"template": "headroom"})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["agent"]["subscription"], "pool-alt");
    assert_eq!(r.body["template"], "headroom-alt");
    let restricted = id(&td
        .agent(
            "p",
            "restricted",
            json!({"policy": {"spawn": {
        "templates": ["headroom"]}}}),
        )
        .await);
    let r = spawn_as(
        &td,
        &restricted,
        &restricted,
        json!({"template": "headroom"}),
    )
    .await;
    assert_eq!(r.status, 409, "{}", r.body);
}

#[tokio::test]
async fn spawn_headroom_ignores_reset_windows_and_full_streams() {
    let (td, _lead, _w) = setup().await;
    let s = state(&td);
    for n in ["local-a", "local-b"] {
        td.post(
            "/api/subscriptions",
            json!({"name": n, "provider": "selfhosted",
            "harnesses": ["pi"], "selfhosted": {"base_url": "http://localhost:1234/v1",
            "max_concurrent": 1}}),
        )
        .await
        .ok();
    }
    td.post(
        "/api/templates",
        json!({"name": "headroom", "project": "p", "spec": {
        "harness": "pi", "subscription": "local-a", "subscription_pool": ["local-b"]}}),
    )
    .await
    .ok();
    s.db.write("system", |t| {
        terrariumd::store::subscriptions::insert_sample(
            t,
            "local-a",
            "5h",
            100.0,
            Some("2000-01-01T00:00:00Z"),
            &json!({}),
        )
    })
    .await
    .unwrap();
    let o = id(&td
        .agent(
            "p",
            "local-owner",
            json!({"policy": {"spawn": {
        "templates": ["headroom"], "max_concurrent": 10}}}),
        )
        .await);
    let r = spawn_as(&td, &o, &o, json!({"template": "headroom"})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["agent"]["subscription"], "local-a");
    let busy = terrarium_core::AgentId::new();
    s.usage.streams.take(Some("local-a"), busy);
    let r = spawn_as(&td, &o, &o, json!({"template": "headroom"})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["agent"]["subscription"], "local-b");
}

/// TER-93 (review finding 1): a spawn gives no `thread:open` past the
/// owner's. A template cannot carry `policy.thread_open` (the template
/// files take `targets`, `mail_to`, and `cross_project` only), and the
/// child of an owner without the right has none.
#[tokio::test]
async fn a_spawn_gives_no_thread_open_past_the_owner() {
    let (td, lead, w) = setup().await;
    let r = td
        .post(
            "/api/templates",
            json!({"name": "opener", "project": "p",
                   "spec": {"harness": "claude", "policy": {"thread_open": true}}}),
        )
        .await;
    assert_eq!(r.status, 422, "{}", r.body);
    assert!(message(&r).contains("policy.thread_open"), "{}", r.body);

    // `w` has spawn rights and no thread:open: its child has none.
    let r = spawn_as(&td, &w, &w, json!({})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_ne!(r.body["agent"]["policy"]["thread_open"], true, "{}", r.body);
    // The lead spawns for `w`: still `w`'s rights.
    let r = spawn_as(&td, &lead, &w, json!({})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_ne!(r.body["agent"]["policy"]["thread_open"], true, "{}", r.body);
}

/// The grants of `secret` to the agent `child`.
async fn grants_to(td: &TestDaemon, secret: &str, child: &str) -> Vec<Value> {
    let key = format!("agent:{child}");
    grants_of(td, secret)
        .await
        .into_iter()
        .filter(|x| x["grantee"] == key)
        .collect()
}

async fn template(td: &TestDaemon, name: &str, grants: Value) {
    let r = td
        .post(
            "/api/templates",
            json!({"name": name, "project": "p",
                   "spec": {"harness": "claude", "grants": grants}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
}

/// TER-88: the `grants` of a template go through the owner check of a
/// spawn, as the body's grants do, with the `run` level and its targets
/// (TER-142). A refusal is 403, names the template and the secret, and
/// makes no agent. A body grant of a secret replaces the template's.
#[tokio::test]
async fn template_grants_are_checked_against_the_owner() {
    let (td, _lead, _w) = setup().await;
    let o = id(&td
        .agent(
            "p",
            "o",
            json!({"policy": {"targets": ["box"],
                              "spawn": {"templates": ["granted", "over", "missing",
                                                      "runs", "runs-past"],
                                        "max_concurrent": 8}}}),
        )
        .await);
    for (s, body) in [
        (
            "global/openai",
            json!({"grantee": "agent:p/o", "level": "read"}),
        ),
        ("global/gh", json!({"grantee": "agent:p/o", "level": "use"})),
        (
            "p/db",
            json!({"grantee": "agent:p/o", "level": "run", "targets": ["box"]}),
        ),
    ] {
        let r = td.post(&format!("/api/secrets/{s}/grants"), body).await;
        assert_eq!(r.status, 201, "{}", r.body);
    }
    template(&td, "granted", json!(["openai", "global/gh:use"])).await;
    template(&td, "over", json!(["gh:read"])).await;
    template(&td, "missing", json!(["other-secret"])).await;
    template(&td, "runs", json!(["db:run"])).await;
    template(&td, "runs-past", json!(["db:run=other"])).await;

    let r = spawn_as(&td, &o, &o, json!({"template": "granted"})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let child = id(&r.body);
    let g = grants_to(&td, "global/openai", &child).await;
    assert_eq!(g.len(), 1, "{g:?}");
    assert_eq!(g[0]["level"], "read");
    let g = grants_to(&td, "global/gh", &child).await;
    assert_eq!(g[0]["level"], "use");
    // The audit row of each grant names the template.
    let audit = td
        .get("/api/audit?action=secret.grant")
        .await
        .body
        .to_string();
    assert!(audit.contains("\"template\":\"granted\""), "{audit}");
    assert!(!audit.contains(KEY) && !audit.contains(TOKEN));

    // A level past the owner's, and a secret that the owner does not hold.
    let before = grants_of(&td, "global/gh").await.len();
    let r = spawn_as(&td, &o, &o, json!({"template": "over"})).await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert!(message(&r).contains("template `over`"), "{}", r.body);
    assert!(message(&r).contains("global/gh"), "{}", r.body);
    assert!(message(&r).contains("level `read`"), "{}", r.body);
    assert_eq!(grants_of(&td, "global/gh").await.len(), before);
    let r = spawn_as(&td, &o, &o, json!({"template": "missing"})).await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert!(message(&r).contains("secret `other-secret`"), "{}", r.body);
    // The body asks for a lower level: it replaces the template's grant.
    let r = spawn_as(
        &td,
        &o,
        &o,
        json!({"template": "over", "grants": ["gh:use"]}),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let g = grants_to(&td, "global/gh", &id(&r.body)).await;
    assert_eq!(g.len(), 1, "{g:?}");
    assert_eq!(g[0]["level"], "use");

    // `:run` on a template: the owner's run targets, never past them.
    let r = spawn_as(&td, &o, &o, json!({"template": "runs"})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let g = grants_to(&td, "p/db", &id(&r.body)).await;
    assert_eq!(g[0]["level"], "run");
    assert_eq!(g[0]["targets"], json!(["box"]));
    let r = spawn_as(&td, &o, &o, json!({"template": "runs-past"})).await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert!(message(&r).contains("template `runs-past`"), "{}", r.body);
    assert!(message(&r).contains("`other`"), "{}", r.body);

    // The operator as owner: a `run` grant with no targets has nothing to
    // take them from.
    let r = td
        .post(
            "/api/agents/operator/spawn",
            json!({"template": "runs", "task": "t", "project": "p"}),
        )
        .await;
    assert_eq!(r.status, 422, "{}", r.body);
}

/// TER-88: a create from a template with grants (not a spawn) needs the
/// caller's right to grant each secret: a project_admin for a secret of
/// its project, a sysadmin or the operator for a global one. A refusal is
/// 403 with the secret named, and makes no agent. A create body takes no
/// `grants`, and a template grant must parse.
#[tokio::test]
async fn a_create_from_a_template_needs_the_right_to_grant_its_secrets() {
    let (td, lead, _w) = setup().await;
    template(&td, "tdb", json!(["db:run=p-pbox"])).await;
    template(&td, "tglobal", json!(["gh"])).await;
    template(&td, "trun", json!(["db:run"])).await;

    let r = td
        .post_as(
            &lead,
            "/api/agents",
            json!({"name": "c1", "project": "p", "template": "tdb"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let g = grants_to(&td, "p/db", &id(&r.body)).await;
    assert_eq!(g.len(), 1, "{g:?}");
    assert_eq!(g[0]["level"], "run");
    assert_eq!(g[0]["targets"], json!(["p-pbox"]));
    let audit = td
        .get("/api/audit?action=secret.grant")
        .await
        .body
        .to_string();
    assert!(audit.contains("\"template\":\"p/tdb\""), "{audit}");

    // A global secret: the project_admin may not grant it.
    let before = grants_of(&td, "global/gh").await.len();
    let r = td
        .post_as(
            &lead,
            "/api/agents",
            json!({"name": "c2", "project": "p", "template": "tglobal"}),
        )
        .await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert!(message(&r).contains("secret `global/gh`"), "{}", r.body);
    assert_eq!(grants_of(&td, "global/gh").await.len(), before);
    // No agent `c2` was made: the operator creates it now.
    let r = td
        .post(
            "/api/agents",
            json!({"name": "c2", "project": "p", "template": "tglobal"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let g = grants_to(&td, "global/gh", &id(&r.body)).await;
    assert_eq!(g[0]["level"], "use");

    // A `run` grant with no targets has no owner to take them from.
    let r = td
        .post(
            "/api/agents",
            json!({"name": "c3", "project": "p", "template": "trun"}),
        )
        .await;
    assert_eq!(r.status, 422, "{}", r.body);
    // The body takes no grants; a template grant must parse.
    let r = td
        .post(
            "/api/agents",
            json!({"name": "c4", "project": "p", "template": "tdb", "grants": ["gh"]}),
        )
        .await;
    assert_eq!(r.status, 422, "{}", r.body);
    let r = td
        .post(
            "/api/templates",
            json!({"name": "bad", "project": "p",
                   "spec": {"harness": "claude", "grants": ["db:write"]}}),
        )
        .await;
    assert_eq!(r.status, 422, "{}", r.body);
}

/// TER-88 review: writing grants into a template needs the right to grant
/// each secret (`policy::grant_secret`), at the create and at any PATCH
/// of a template with grants (round 2 finding 1). A project_admin may put
/// a secret of its project into a template of its project, not a global
/// secret. A grant with no scope counts as the secret that it names now.
/// A PATCH that changes nothing checks nothing; one that drops all the
/// grants is fine. The writer becomes `grants_by`.
#[tokio::test]
async fn writing_template_grants_needs_the_right_to_grant_them() {
    let (td, lead, _w) = setup().await;
    let new = |name: &str, grants: Value| {
        json!({"name": name, "project": "p",
               "spec": {"harness": "claude", "grants": grants}})
    };
    // A global secret, scoped or by its name only: refused, no template.
    for (name, grants) in [
        ("tg1", json!(["global/openai:read"])),
        ("tg2", json!(["gh"])),
    ] {
        let r = td.post_as(&lead, "/api/templates", new(name, grants)).await;
        assert_eq!(r.status, 403, "{}", r.body);
        assert!(message(&r).contains("template `p/"), "{}", r.body);
        let secret = if name == "tg1" {
            "global/openai"
        } else {
            "global/gh"
        };
        assert!(
            message(&r).contains(&format!("secret `{secret}`")),
            "{}",
            r.body
        );
        let list = td.get("/api/templates?project=p").await.body.to_string();
        assert!(!list.contains(&format!("\"{name}\"")), "{list}");
    }
    // A secret of its own project: fine.
    let r = td
        .post_as(&lead, "/api/templates", new("tp", json!(["db:run=p-pbox"])))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["grants_by"], format!("agent:{lead}"), "{}", r.body);

    // The operator's template with a global grant: any write by the
    // project_admin that keeps a grant is refused and names the grant (a
    // new prompt would run with the operator's secret). A write that
    // changes nothing is fine. One that drops all the grants is fine.
    let r = td
        .post(
            "/api/templates",
            json!({"name": "op", "project": "p", "description": "d0",
                   "spec": {"harness": "claude", "prompt": "worker",
                            "grants": ["global/openai:read"]}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["grants_by"], "operator", "{}", r.body);
    let tid = id(&r.body);
    let path = format!("/api/templates/{tid}");
    let same = json!({"harness": "claude", "prompt": "worker",
                      "grants": ["global/openai:read"]});
    for patch in [
        json!({"spec": {"harness": "claude", "prompt": "mine",
                        "grants": ["global/openai:read"]}}),
        json!({"description": "d"}),
        json!({"name": "op2"}),
        json!({"spec": {"harness": "claude", "prompt": "worker",
                        "grants": ["global/openai:use"]}}),
    ] {
        let r = td.patch_as(&lead, &path, patch.clone()).await;
        assert_eq!(r.status, 403, "{patch}: {}", r.body);
        assert!(
            message(&r).contains("the grant `global/openai:"),
            "{}",
            r.body
        );
        assert!(message(&r).contains("secret `global/openai`"), "{}", r.body);
        let shown = td.get(&path).await;
        let shown = shown.ok();
        assert_eq!(shown["spec"]["prompt"], "worker", "{shown}");
        assert_eq!(
            shown["spec"]["grants"],
            json!(["global/openai:read"]),
            "{shown}"
        );
        assert_eq!(shown["name"], "op", "{shown}");
        assert_eq!(shown["description"], "d0", "{shown}");
        assert_eq!(shown["grants_by"], "operator", "{shown}");
    }
    // The same values: no write, nothing to check.
    let r = td
        .patch_as(&lead, &path, json!({"description": "d0", "spec": same}))
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["grants_by"], "operator", "{}", r.body);
    // A sysadmin may change the prompt; it becomes grants_by.
    let sys = id(&td.agent("p", "sys", json!({"roles": ["sysadmin"]})).await);
    let r = td
        .patch_as(
            &sys,
            &path,
            json!({"spec": {"harness": "claude", "prompt": "sysworker",
                            "grants": ["global/openai:read"]}}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["grants_by"], format!("agent:{sys}"), "{}", r.body);
    assert_eq!(r.body["spec"]["prompt"], "sysworker", "{}", r.body);
    // The project_admin's new prompt that also drops all the grants.
    let r = td
        .patch_as(
            &lead,
            &path,
            json!({"spec": {"harness": "claude", "prompt": "mine"}}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["spec"]["prompt"], "mine", "{}", r.body);
    assert!(r.body["spec"].get("grants").is_none(), "{}", r.body);
    assert!(r.body.get("grants_by").is_none(), "{}", r.body);
}

/// TER-88 review: a spawn and a create check again that the one who set
/// a template's grants may grant each secret now. A sysadmin agent sets a
/// global grant; once it lost the role, the template gives the grant to
/// no one, also when the owner holds the secret and the creator is the
/// operator.
#[tokio::test]
async fn a_spawn_checks_again_who_set_the_template_grants() {
    let (td, _lead, _w) = setup().await;
    let sys = id(&td.agent("p", "sys", json!({"roles": ["sysadmin"]})).await);
    let o = id(&td
        .agent(
            "p",
            "o",
            json!({"policy": {"spawn": {"templates": ["bysys"], "max_concurrent": 8}}}),
        )
        .await);
    let r = td
        .post(
            "/api/secrets/global/openai/grants",
            json!({"grantee": "agent:p/o", "level": "read"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let r = td
        .post_as(
            &sys,
            "/api/templates",
            json!({"name": "bysys", "project": "p",
                   "spec": {"harness": "claude", "grants": ["global/openai:read"]}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let r = spawn_as(&td, &o, &o, json!({"template": "bysys"})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(grants_to(&td, "global/openai", &id(&r.body)).await.len(), 1);

    let r = td
        .put(&format!("/api/agents/{sys}/roles"), json!({"roles": []}))
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let before = grants_of(&td, "global/openai").await.len();
    let r = spawn_as(&td, &o, &o, json!({"template": "bysys"})).await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert!(message(&r).contains("template `bysys`"), "{}", r.body);
    assert!(message(&r).contains("secret `global/openai`"), "{}", r.body);
    assert!(message(&r).contains(&format!("agent:{sys}")), "{}", r.body);
    let r = td
        .post(
            "/api/agents",
            json!({"name": "c1", "project": "p", "template": "bysys"}),
        )
        .await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert!(message(&r).contains("secret `global/openai`"), "{}", r.body);
    assert_eq!(grants_of(&td, "global/openai").await.len(), before);
    let r = td.get("/api/agents/p%2Fc1").await;
    assert_eq!(r.status, 404, "{}", r.body);
}

/// TER-88 review: grants are given at the spawn only. A later change of
/// the template's grants does not change the grants of a running child.
#[tokio::test]
async fn a_template_edit_does_not_change_a_running_childs_grants() {
    let (td, _lead, _w) = setup().await;
    let o = id(&td
        .agent(
            "p",
            "o",
            json!({"policy": {"spawn": {"templates": ["edited"], "max_concurrent": 8}}}),
        )
        .await);
    for s in ["global/openai", "global/gh"] {
        let r = td
            .post(
                &format!("/api/secrets/{s}/grants"),
                json!({"grantee": "agent:p/o", "level": "read"}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
    }
    let r = td
        .post(
            "/api/templates",
            json!({"name": "edited", "project": "p",
                   "spec": {"harness": "claude", "grants": ["openai:read"]}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let path = format!("/api/templates/{}", id(&r.body));
    let r = spawn_as(&td, &o, &o, json!({"template": "edited"})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let child = id(&r.body);

    for grants in [json!(["openai:use", "gh:read"]), json!([])] {
        let r = td
            .patch(
                &path,
                json!({"spec": {"harness": "claude", "grants": grants}}),
            )
            .await;
        assert_eq!(r.status, 200, "{}", r.body);
        let g = grants_to(&td, "global/openai", &child).await;
        assert_eq!(g.len(), 1, "{g:?}");
        assert_eq!(g[0]["level"], "read");
        assert!(grants_to(&td, "global/gh", &child).await.is_empty());
    }
}

/// TER-88 review: a body grant replaces the template's grant of the same
/// secret, compared by the resolved secret. A template `gh` that resolves
/// to the project's `p/gh` stays next to a body `global/gh`.
#[tokio::test]
async fn a_body_grant_replaces_only_the_same_resolved_secret() {
    let (td, _lead, _w) = setup().await;
    let r = td
        .put(
            "/api/secrets/p/gh",
            json!({"kind": "api_key", "fields": {"value": KEY}}),
        )
        .await;
    assert!(r.status.is_success(), "{}", r.body);
    let o = id(&td
        .agent(
            "p",
            "o",
            json!({"policy": {"spawn": {"templates": ["tgh"], "max_concurrent": 8}}}),
        )
        .await);
    for s in ["p/gh", "global/gh"] {
        let r = td
            .post(
                &format!("/api/secrets/{s}/grants"),
                json!({"grantee": "agent:p/o", "level": "use"}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
    }
    template(&td, "tgh", json!(["gh:use"])).await;
    let r = spawn_as(
        &td,
        &o,
        &o,
        json!({"template": "tgh", "grants": ["global/gh:use"]}),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let child = id(&r.body);
    assert_eq!(grants_to(&td, "p/gh", &child).await.len(), 1);
    assert_eq!(grants_to(&td, "global/gh", &child).await.len(), 1);
    // The same secret in the body: one grant, the body's.
    let r = spawn_as(
        &td,
        &o,
        &o,
        json!({"template": "tgh", "grants": ["p/gh:use"]}),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let child = id(&r.body);
    assert_eq!(grants_to(&td, "p/gh", &child).await.len(), 1);
    assert!(grants_to(&td, "global/gh", &child).await.is_empty());
}
