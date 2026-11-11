//! SSH keys for targets (PLAN §16b.6a, docs/api.md §5.2 and §17): the
//! save-time check of a target's `credential` (scope and kind), the refusal
//! to delete a secret that a target uses, `secret generate`, the
//! fingerprint in the secret and the target view, and the refusals of
//! `target test`. The ssh results of `target test` are in the unit tests
//! of `terrariumd::target_test` (a fake `ssh`, no network).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::{TestDaemon, write};
use serde_json::{Value, json};
use ssh_key::{HashAlg, PrivateKey, PublicKey};
use terrariumd::secrets::{self, Secret};

#[tokio::test]
async fn patch_address_keeps_the_new_pin() {
    let td = TestDaemon::start().await;
    td.post(
        "/api/targets",
        json!({"name": "dns-box", "kind": "ssh", "address": "old.invalid"}),
    )
    .await
    .ok();
    td.patch("/api/targets/dns-box", json!({"address": "localhost"}))
        .await
        .ok();
    // The PATCH's own lookup is best effort (TER-367: it skips when the
    // scheduler holds the pin lock), and the scheduler pins the name on its
    // next tick. Wait for the pin, not for the PATCH (TER-827).
    let conn = terrariumd::db::open(&td.state.join("terrarium.db")).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    let key: String = loop {
        let row = conn.query_row(
            "SELECT host_key FROM job_host_keys WHERE hostname = 'localhost'",
            [],
            |r| r.get(0),
        );
        match row {
            Ok(key) => break key,
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the new address was not pinned within 90 s"
                );
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(e) => panic!("{e}"),
        }
    };
    assert!(key.parse::<std::net::IpAddr>().unwrap().is_loopback());
}

async fn generate(td: &TestDaemon, scope: &str, name: &str, body: Value) -> Value {
    let r = td
        .post(&format!("/api/secrets/{scope}/{name}/generate"), body)
        .await;
    assert_eq!(r.status.as_u16(), 201, "{}", r.body);
    r.body
}

async fn login(td: &TestDaemon, scope: &str, name: &str) {
    let r = td
        .put(
            &format!("/api/secrets/{scope}/{name}"),
            json!({"kind": "login", "fields": {"username": "admin", "password": "pw-s3cret-pw"}}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 201, "{}", r.body);
}

fn ssh(name: &str, project: Option<&str>, cred: &str) -> Value {
    let mut v = json!({"name": name, "kind": "ssh", "address": "build@box", "credential": cred});
    if let Some(p) = project {
        v["project"] = p.into();
    }
    v
}

fn display(name: &str, kind: &str, project: Option<&str>, cred: &str) -> Value {
    let mut v = json!({"name": name, "kind": kind, "host": "lab1", "address": "10.0.0.5",
                       "credential": cred});
    if let Some(p) = project {
        v["project"] = p.into();
    }
    v
}

#[tokio::test]
async fn a_target_credential_must_name_a_secret_of_its_kind_and_scope() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.project("q").await;
    let gk = generate(&td, "global", "gk", json!({})).await;
    generate(&td, "p", "pk", json!({})).await;
    generate(&td, "q", "qk", json!({})).await;
    login(&td, "global", "gl").await;
    login(&td, "p", "pl").await;
    let r = td
        .post(
            "/api/secrets",
            json!({"name": "tok", "kind": "token", "fields": {"value": "ghp_tokens3cretValue"}}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 201);

    // (target, ok?, a part of the message)
    let cases: Vec<(Value, bool, &str)> = vec![
        // A global ssh target: a global ssh_key only.
        (ssh("g-gk", None, "gk"), true, ""),
        (
            ssh("g-pk", None, "pk"),
            false,
            "a global target can use a global secret only",
        ),
        (
            ssh("g-gl", None, "gl"),
            false,
            "is of kind login; ssh targets need kind ssh_key",
        ),
        (ssh("g-tok", None, "tok"), false, "is of kind token"),
        (
            ssh("g-none", None, "none"),
            false,
            "there is no secret `none` (global)",
        ),
        // A project ssh target: its project's or a global ssh_key.
        (ssh("pk", Some("p"), "pk"), true, ""),
        (ssh("gk", Some("p"), "gk"), true, ""),
        (
            ssh("qk", Some("p"), "qk"),
            false,
            "no secret `qk` (project `p` or global)",
        ),
        (ssh("pl", Some("p"), "pl"), false, "is of kind login"),
        // vnc and rdp: a login, with the same scope rule.
        (display("g-vnc", "vnc", None, "gl"), true, ""),
        (
            display("g-vnc-k", "vnc", None, "gk"),
            false,
            "is of kind ssh_key; vnc targets need kind login",
        ),
        (
            display("g-rdp-p", "rdp", None, "pl"),
            false,
            "global secret only",
        ),
        (display("rdp", "rdp", Some("p"), "pl"), true, ""),
        (display("rdp-g", "rdp", Some("p"), "gl"), true, ""),
        (
            display("rdp-k", "rdp", Some("p"), "pk"),
            false,
            "rdp targets need kind login",
        ),
    ];
    for (body, ok, part) in cases {
        let r = td.post("/api/targets", body.clone()).await;
        if ok {
            assert_eq!(r.status.as_u16(), 201, "{body}: {}", r.body);
        } else {
            assert_eq!(r.status.as_u16(), 422, "{body}: {}", r.body);
            let m = r.body["error"]["message"].as_str().unwrap_or_default();
            assert!(m.contains(part), "{body}: `{part}` not in `{m}`");
        }
    }

    // The view names the key and its fingerprint.
    let v = td.get("/api/targets/g-gk").await;
    let c = &v.ok()["credential_info"];
    assert_eq!(c["name"], "gk");
    assert_eq!(c["scope"], "global");
    assert_eq!(c["kind"], "ssh_key");
    assert_eq!(c["fingerprint"], gk["fields"]["fingerprint"]);
    assert!(c.get("warning").is_none(), "{c}");
    // A project secret hides the global one of the same name.
    generate(&td, "p", "gk", json!({})).await;
    let v = td.get("/api/targets/p-gk").await;
    assert_eq!(v.ok()["credential_info"]["scope"], "p");
    // The list has it too; a target with no credential has none.
    let list = td.get("/api/targets").await;
    let rows = list.ok().as_array().unwrap();
    let row = rows.iter().find(|t| t["name"] == "p-pk").unwrap();
    assert_eq!(row["credential_info"]["kind"], "ssh_key");

    // An edit checks a changed credential, and lets other changes pass.
    let r = td
        .patch("/api/targets/g-gk", json!({"credential": "pk"}))
        .await;
    assert_eq!(r.status.as_u16(), 422, "{}", r.body);
    let r = td
        .patch("/api/targets/p-pk", json!({"credential": "qk"}))
        .await;
    assert_eq!(r.status.as_u16(), 422, "{}", r.body);
    let r = td
        .patch("/api/targets/p-pk", json!({"credential": "gk"}))
        .await;
    assert_eq!(r.status.as_u16(), 200, "{}", r.body);
    // Moving a project target to global with a project key is refused.
    let r = td
        .patch("/api/targets/p-pk", json!({"credential": "pk"}))
        .await;
    assert_eq!(r.status.as_u16(), 200, "{}", r.body);
    let r = td
        .patch("/api/targets/p-pk", json!({"project": null}))
        .await;
    assert_eq!(r.status.as_u16(), 422, "{}", r.body);
}

#[tokio::test]
async fn a_secret_that_a_target_uses_is_deleted_only_with_force() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    generate(&td, "global", "deploy", json!({})).await;
    generate(&td, "p", "own", json!({})).await;
    td.post("/api/targets", ssh("box", None, "deploy"))
        .await
        .ok();
    td.post("/api/targets", ssh("box", Some("p"), "deploy"))
        .await
        .ok();
    td.post("/api/targets", ssh("own", Some("p"), "own"))
        .await
        .ok();

    let r = td.delete("/api/secrets/global/deploy").await;
    r.err(409, "conflict");
    assert_eq!(
        r.body["error"]["details"]["targets"],
        json!(["box", "p-box"])
    );
    let m = r.body["error"]["message"].as_str().unwrap();
    assert!(
        m.contains("`box`") && m.contains("`p-box`") && m.contains("force=true"),
        "{m}"
    );
    td.delete("/api/secrets/p/own").await.err(409, "conflict");
    // The old alias refuses too.
    td.delete("/api/credentials/deploy")
        .await
        .err(409, "conflict");

    // A project secret of the same name takes `p-box` off the global one.
    generate(&td, "p", "deploy", json!({})).await;
    let r = td.delete("/api/secrets/global/deploy").await;
    assert_eq!(r.body["error"]["details"]["targets"], json!(["box"]));

    // With force, the target keeps the name and shows a warning.
    let r = td.delete("/api/secrets/global/deploy?force=true").await;
    assert_eq!(r.status.as_u16(), 204, "{}", r.body);
    let v = td.get("/api/targets/box").await;
    let c = &v.ok()["credential_info"];
    assert_eq!(v.body["credential"], "deploy");
    assert_eq!(c["scope"], Value::Null);
    assert!(
        c["warning"]
            .as_str()
            .unwrap()
            .contains("there is no secret `deploy`"),
        "{c}"
    );
    let audit = td.get("/api/audit?action=secret.delete").await;
    assert_eq!(audit.ok()[0]["detail"]["targets"], json!(["box"]));
    // A target test refuses a target whose credential does not work.
    let r = td.post_empty("/api/targets/box/test").await;
    r.err(422, "invalid");
    // The unused secret goes with no question.
    td.delete("/api/targets/p-own").await;
    let r = td.delete("/api/secrets/p/own").await;
    assert_eq!(r.status.as_u16(), 204, "{}", r.body);
}

/// Open the stored private key as the daemon does, and sign with it.
fn sign_with_store(td: &TestDaemon, scope: &str, name: &str, public: &str) {
    let conn = secrets::open_db(&td.state).unwrap();
    let key = secrets::key(&td.state).unwrap();
    let opened = secrets::open(&conn, &key, scope, name).unwrap().unwrap();
    let Secret::SshKey(pem) = opened.to_secret().unwrap() else {
        panic!("not an ssh key");
    };
    let private = PrivateKey::from_openssh(pem.trim()).unwrap();
    let sig = private
        .sign("terrarium-test", HashAlg::Sha512, b"hello")
        .unwrap();
    let public = PublicKey::from_openssh(public).unwrap();
    public.verify("terrarium-test", b"hello", &sig).unwrap();
}

#[tokio::test]
async fn generate_makes_a_key_pair_and_shows_only_the_public_key() {
    let td = TestDaemon::start().await;
    td.project("p").await;

    // ed25519 by default, with no body.
    let r = td.post_empty("/api/secrets/global/deploy/generate").await;
    assert_eq!(r.status.as_u16(), 201, "{}", r.body);
    let info = r.body;
    assert_eq!(info["kind"], "ssh_key");
    assert_eq!(info["secret_fields"], json!(["private_key"]));
    let public = info["fields"]["public_key"].as_str().unwrap();
    assert!(public.starts_with("ssh-ed25519 AAAA"), "{public}");
    assert!(public.ends_with(" terrarium:global/deploy"), "{public}");
    let fp = info["fields"]["fingerprint"].as_str().unwrap();
    assert!(fp.starts_with("SHA256:") && fp.len() == 50, "{fp}");
    let parsed = PublicKey::from_openssh(public).unwrap();
    assert_eq!(parsed.fingerprint(HashAlg::Sha256).to_string(), fp);
    assert!(!info.to_string().contains("PRIVATE KEY"), "{info}");
    sign_with_store(&td, "global", "deploy", public);

    // rsa, in a project, with a comment.
    let info = generate(
        &td,
        "p",
        "rsa",
        json!({"type": "rsa", "bits": 3072, "comment": "ci@p"}),
    )
    .await;
    let public = info["fields"]["public_key"].as_str().unwrap();
    assert!(
        public.starts_with("ssh-rsa AAAA") && public.ends_with(" ci@p"),
        "{public}"
    );
    assert_eq!(info["scope"], "p");
    sign_with_store(&td, "p", "rsa", public);

    // A name that exists: 409, unless `replace`; then a new key.
    td.post("/api/secrets/global/deploy/generate", json!({}))
        .await
        .err(409, "conflict");
    let r = td
        .post(
            "/api/secrets/global/deploy/generate",
            json!({"replace": true}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 200, "{}", r.body);
    assert_ne!(r.body["fields"]["fingerprint"], fp);
    // Bad bodies.
    td.post(
        "/api/secrets/global/x/generate",
        json!({"type": "rsa", "bits": 1024}),
    )
    .await
    .err(422, "invalid");
    td.post("/api/secrets/global/x/generate", json!({"type": "dsa"}))
        .await
        .err(400, "bad_request");
    td.post("/api/secrets/nope/x/generate", json!({}))
        .await
        .err(404, "not_found");

    // The audit row says `generated` and holds no key.
    let audit = td.get("/api/audit?action=secret.put").await;
    let rows = audit.ok().as_array().unwrap();
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert!(
        rows.iter().all(|r| r["detail"]["generated"] == true),
        "{rows:?}"
    );
    assert!(!audit.body.to_string().contains("PRIVATE KEY"));

    // A member agent may not generate (the rights of a secret create).
    td.agent("p", "dev", json!({})).await;
    let r = td
        .post_as("p/dev", "/api/secrets/p/mine/generate", json!({}))
        .await;
    r.err(403, "forbidden");
}

#[tokio::test]
async fn target_test_is_for_ssh_targets_only() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.post(
        "/api/targets",
        json!({"name": "h", "kind": "host", "host": "lab1"}),
    )
    .await
    .ok();
    let r = td.post_empty("/api/targets/h/test").await;
    r.err(422, "invalid");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not supported"),
        "{}",
        r.body
    );
    td.post_empty("/api/targets/nope/test")
        .await
        .err(404, "not_found");
    // An agent that may not use or define the target cannot test it.
    generate(&td, "p", "k", json!({})).await;
    td.post("/api/targets", ssh("box", Some("p"), "k"))
        .await
        .ok();
    td.agent("p", "dev", json!({})).await;
    td.post_empty_as("p/dev", "/api/targets/p-box/test")
        .await
        .err(403, "forbidden");
}

#[tokio::test]
async fn an_old_file_target_with_a_bad_credential_still_loads_and_warns() {
    let td = TestDaemon::with_config(|c| {
        write(
            c,
            "targets/old.toml",
            "kind = \"ssh\"\naddress = \"build@box\"\ncredential = \"gone\"\n",
        );
    })
    .await;
    let v = td.get("/api/targets/old").await;
    assert_eq!(v.ok()["credential"], "gone");
    assert!(
        v.body["credential_info"]["warning"]
            .as_str()
            .unwrap()
            .contains("no secret `gone`"),
        "{}",
        v.body
    );
    let r = td.post_empty("/api/reload").await;
    let errors = r.ok()["errors"].as_array().unwrap();
    assert!(
        errors.iter().any(|e| e
            .as_str()
            .unwrap()
            .starts_with("targets/old.toml: warning:")),
        "{errors:?}"
    );
    // The secret comes; the warning goes.
    generate(&td, "global", "gone", json!({})).await;
    let v = td.get("/api/targets/old").await;
    assert!(
        v.ok()["credential_info"].get("warning").is_none(),
        "{}",
        v.body
    );
    assert!(v.body["credential_info"]["fingerprint"].is_string());
}
