//! Board rules files (TER-166, docs/api.md §19.8): the schema check on
//! each write, the rights (read: the project; write: the operator or a
//! project admin), the audit row of each change, the stale `base_hash`,
//! the file in the config dir at the start and at a reload, and the
//! export to the config repo.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{TestDaemon, write};
use serde_json::{Value, json};
use terrariumd::config_repo::ConfigRepo;

// The verbs are not in the order of the schema (card_move, card_label,
// card_assign): a read that grouped them by kind fails the order checks.
const GOOD: &str = r#"rule "review-pass" {
  on = ["job"]
  when "data.outcome" { eq = "passed" }
  card_assign { who = "none" }
  card_move { to = "review"  comment = "Job {{job.id}} passed" }
  card_label { remove = ["validation:running"] }
}
"#;

/// The verbs of the first rule of a rules body.
fn verbs_of(body: &Value) -> Vec<String> {
    body["rules"][0]["actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["verb"].as_str().unwrap().to_owned())
        .collect()
}

const OTHER: &str = r#"rule "block-on-fail" {
  on = ["job"]
  when "data.outcome" { eq = "failed" }
  card_move { to = "@blocked"  comment = "Job {{job.id}} failed" }
}
"#;

/// The audit rows of `action`, newest first.
async fn audit_rows(td: &TestDaemon, action: &str) -> Vec<Value> {
    let r = td.get("/api/audit?order=desc&limit=200").await;
    r.ok()
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["action"] == action)
        .cloned()
        .collect()
}

#[tokio::test]
async fn put_get_check_delete_with_audit() {
    let td = TestDaemon::start().await;
    td.project("terra").await;

    // No rules yet.
    let r = td.get("/api/projects/terra/rules").await;
    assert_eq!(r.ok()["exists"], false);
    assert_eq!(r.body["rules"], json!([]));

    // The schema the daemon ships.
    let s = td.get("/api/rules/schema").await;
    assert!(s.ok()["schema"].as_str().unwrap().contains("@document"));
    assert!(
        s.body["verbs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["verb"] == "card.move")
    );

    // A good file goes in, read back in file order.
    let r = td
        .put(
            "/api/projects/terra/rules",
            json!({"text": GOOD, "base_hash": ""}),
        )
        .await;
    let body = r.ok().clone();
    assert_eq!(body["exists"], true);
    assert_eq!(body["source"], "api");
    assert_eq!(body["text"], GOOD);
    assert_eq!(body["rules"][0]["name"], "review-pass");
    assert_eq!(verbs_of(&body), ["card.assign", "card.move", "card.label"]);
    let hash = body["hash"].as_str().unwrap().to_owned();
    let r = td.get("/api/projects/terra/rules").await;
    assert_eq!(r.ok()["hash"], hash);
    assert_eq!(
        verbs_of(&r.body),
        ["card.assign", "card.move", "card.label"]
    );

    // The audit row names the rules and the hashes.
    let rows = audit_rows(&td, "rules.put").await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["target"], "terra");
    assert_eq!(rows[0]["project"], "terra");
    assert_eq!(rows[0]["actor"], "operator");
    assert_eq!(rows[0]["detail"]["hash"], hash);
    assert_eq!(rows[0]["detail"]["previous_hash"], Value::Null);
    assert_eq!(rows[0]["detail"]["rules"], json!(["review-pass"]));

    // A stale base hash is a conflict, and writes nothing.
    td.put(
        "/api/projects/terra/rules",
        json!({"text": OTHER, "base_hash": "0000"}),
    )
    .await
    .err(409, "conflict");
    td.put(
        "/api/projects/terra/rules",
        json!({"text": OTHER, "base_hash": ""}),
    )
    .await
    .err(409, "conflict");
    // The current hash replaces the rules.
    let r = td
        .put(
            "/api/projects/terra/rules",
            json!({"text": OTHER, "base_hash": hash}),
        )
        .await;
    let new_hash = r.ok()["hash"].as_str().unwrap().to_owned();
    let rows = audit_rows(&td, "rules.put").await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["detail"]["previous_hash"], hash);
    assert_eq!(rows[0]["detail"]["rules"], json!(["block-on-fail"]));

    // A bad file: 422 with the problems, and the rules stay.
    let bad = "rule \"r\" {\n  on = [\"job\"]\n  card_frobnicate {}\n}\n";
    let r = td
        .put("/api/projects/terra/rules", json!({"text": bad}))
        .await;
    r.err(422, "invalid");
    let p = &r.body["error"]["details"]["problems"][0];
    assert_eq!(p["line"], 3, "{}", r.body);
    assert!(
        p["message"].as_str().unwrap().contains("card_frobnicate"),
        "{}",
        r.body
    );
    // A column that no default board has (TER-165 adds `accepted`).
    let unknown_column = "rule \"r\" {\n  on = [\"job\"]\n  card_move { to = \"shipped\" }\n}\n";
    let r = td
        .put("/api/projects/terra/rules", json!({"text": unknown_column}))
        .await;
    r.err(422, "invalid");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown column `shipped`"),
        "{}",
        r.body
    );
    let import = "import \"./other.wcl\"\n";
    let r = td
        .put("/api/projects/terra/rules", json!({"text": import}))
        .await;
    r.err(422, "invalid");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("imports are off"),
        "{}",
        r.body
    );
    assert_eq!(
        td.get("/api/projects/terra/rules").await.ok()["hash"],
        new_hash
    );
    assert_eq!(audit_rows(&td, "rules.put").await.len(), 2);

    // A check writes nothing.
    let r = td
        .post("/api/projects/terra/rules/check", json!({"text": GOOD}))
        .await;
    assert_eq!(r.ok()["rules"][0]["name"], "review-pass");
    td.post("/api/projects/terra/rules/check", json!({"text": bad}))
        .await
        .err(422, "invalid");
    // Plain data only: a function call, a let, and a lambda are refused
    // before WCL evaluates them.
    for (text, what) in [
        (
            "rule \"r\" {\n  on = [\"job\"]\n  card_comment { text = join(map(range(0, 1048576), fn(i: i64) -> utf8 { repeat(\"x\", 67108864) }), \"\") }\n}\n",
            "a function call is not allowed",
        ),
        (
            "let x = \"review\"\nrule \"r\" {\n  on = [\"job\"]\n  card_move { to = x }\n}\n",
            "a `let` binding",
        ),
        (
            "rule \"r\" {\n  on = [\"job\"]\n  description = fn(i: i64) -> i64 { i }\n  job_cancel {}\n}\n",
            "a function is not allowed",
        ),
    ] {
        let r = td
            .post("/api/projects/terra/rules/check", json!({"text": text}))
            .await;
        r.err(422, "invalid");
        assert!(
            r.body["error"]["message"].as_str().unwrap().contains(what),
            "{}",
            r.body
        );
        td.put("/api/projects/terra/rules", json!({"text": text}))
            .await
            .err(422, "invalid");
    }
    assert_eq!(
        td.get("/api/projects/terra/rules").await.ok()["hash"],
        new_hash
    );

    // Delete, with its audit row; a second delete is a 404.
    td.delete("/api/projects/terra/rules?base_hash=0000")
        .await
        .err(409, "conflict");
    let r = td
        .delete(&format!("/api/projects/terra/rules?base_hash={new_hash}"))
        .await;
    assert_eq!(r.status.as_u16(), 204, "{}", r.body);
    assert_eq!(
        td.get("/api/projects/terra/rules").await.ok()["exists"],
        false
    );
    let rows = audit_rows(&td, "rules.delete").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["detail"]["previous_hash"], new_hash);
    td.delete("/api/projects/terra/rules")
        .await
        .err(404, "not_found");

    // An unknown project.
    td.get("/api/projects/nope/rules")
        .await
        .err(404, "not_found");
    td.put("/api/projects/nope/rules", json!({"text": GOOD}))
        .await
        .err(404, "not_found");
}

#[tokio::test]
async fn rights_read_the_project_and_write_as_admin() {
    let td = TestDaemon::start().await;
    td.project("terra").await;
    td.project("other").await;
    let lead = td
        .agent("terra", "lead", json!({"roles": ["project_admin"]}))
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let worker = td.agent("terra", "worker", json!({})).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let stranger = td
        .agent("other", "lead", json!({"roles": ["project_admin"]}))
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // The project admin writes; the audit row has its actor.
    let r = td
        .put_as(&lead, "/api/projects/terra/rules", json!({"text": GOOD}))
        .await;
    assert_eq!(r.ok()["updated_by"], format!("agent:{lead}"));
    let rows = audit_rows(&td, "rules.put").await;
    assert_eq!(rows[0]["actor"], format!("agent:{lead}"));

    // A worker of the project reads and checks, but does not write.
    let r = td.get_as(&worker, "/api/projects/terra/rules").await;
    assert_eq!(r.ok()["rules"][0]["name"], "review-pass");
    td.post_as(
        &worker,
        "/api/projects/terra/rules/check",
        json!({"text": OTHER}),
    )
    .await
    .ok();
    td.put_as(&worker, "/api/projects/terra/rules", json!({"text": OTHER}))
        .await
        .err(403, "forbidden");
    let r = td
        .req(
            hyper::Method::DELETE,
            "/api/projects/terra/rules",
            None,
            Some(&worker),
        )
        .await;
    r.err(403, "forbidden");

    // The admin of another project neither reads nor writes.
    td.get_as(&stranger, "/api/projects/terra/rules")
        .await
        .err(403, "forbidden");
    td.put_as(
        &stranger,
        "/api/projects/terra/rules",
        json!({"text": OTHER}),
    )
    .await
    .err(403, "forbidden");

    // Only the one write is in the audit.
    assert_eq!(audit_rows(&td, "rules.put").await.len(), 1);
}

#[tokio::test]
async fn a_rules_file_loads_at_start_and_at_reload() {
    let td = TestDaemon::with_config(|c| {
        write(c, "projects/filed.toml", "description = \"x\"\n");
        write(c, "projects/filed.rules.wcl", GOOD);
    })
    .await;
    let r = td.get("/api/projects/filed/rules").await;
    assert_eq!(r.ok()["source"], "file");
    assert_eq!(r.body["rules"][0]["name"], "review-pass");
    let rows = audit_rows(&td, "rules.file").await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["detail"]["file"], "projects/filed.rules.wcl");

    // The API does not change the rules of a file.
    td.put("/api/projects/filed/rules", json!({"text": OTHER}))
        .await
        .err(409, "file_managed");
    td.delete("/api/projects/filed/rules")
        .await
        .err(409, "file_managed");

    // A reload with no change writes nothing.
    td.post_empty("/api/reload").await.ok();
    assert_eq!(audit_rows(&td, "rules.file").await.len(), 1);

    // A bad file keeps the last good rules, and the reload names it.
    write(
        &td.config,
        "projects/filed.rules.wcl",
        "rule \"r\" {\n  on = [\"job\"]\n  card_move { to = \"nowhere\" }\n}\n",
    );
    let r = td.post_empty("/api/reload").await;
    let errors = r.ok()["errors"].to_string();
    assert!(
        errors.contains("projects/filed.rules.wcl:3:") && errors.contains("unknown column"),
        "{errors}"
    );
    let r = td.get("/api/projects/filed/rules").await;
    assert_eq!(r.ok()["rules"][0]["name"], "review-pass");

    // A good change loads, with an audit row.
    write(&td.config, "projects/filed.rules.wcl", OTHER);
    td.post_empty("/api/reload").await.ok();
    let r = td.get("/api/projects/filed/rules").await;
    assert_eq!(r.ok()["rules"][0]["name"], "block-on-fail");
    assert_eq!(audit_rows(&td, "rules.file").await.len(), 2);

    // The file goes: the rules stay, as API rules, with an audit row.
    std::fs::remove_file(td.config.join("projects/filed.rules.wcl")).unwrap();
    td.post_empty("/api/reload").await.ok();
    let r = td.get("/api/projects/filed/rules").await;
    assert_eq!(r.ok()["source"], "api");
    assert_eq!(r.body["rules"][0]["name"], "block-on-fail");
    let rows = audit_rows(&td, "rules.file").await;
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(rows[0]["detail"]["file"], Value::Null);
    assert_eq!(rows[0]["detail"]["source"], "api");
    assert_eq!(
        rows[0]["detail"]["previous_file"],
        "projects/filed.rules.wcl"
    );
    assert_eq!(rows[0]["detail"]["hash"], r.body["hash"]);
    // A second reload writes no second row.
    td.post_empty("/api/reload").await.ok();
    assert_eq!(audit_rows(&td, "rules.file").await.len(), 3);
    td.put("/api/projects/filed/rules", json!({"text": GOOD}))
        .await
        .ok();

    // A rules file of no project is an error of the reload.
    write(&td.config, "projects/ghost.rules.wcl", GOOD);
    let r = td.post_empty("/api/reload").await;
    assert!(
        r.ok()["errors"].to_string().contains("no project `ghost`"),
        "{}",
        r.body
    );
}

#[tokio::test]
async fn a_broken_file_at_start_does_not_stop_a_good_project() {
    // A file that does not check (a function call) next to a good one.
    let broken =
        "rule \"r\" {\n  on = [\"job\"]\n  description = upper(\"x\")\n  job_cancel {}\n}\n";
    let td = TestDaemon::with_config(|c| {
        write(c, "projects/good.toml", "description = \"x\"\n");
        write(c, "projects/good.rules.wcl", GOOD);
        write(c, "projects/broken.toml", "description = \"y\"\n");
        write(c, "projects/broken.rules.wcl", broken);
    })
    .await;
    // The daemon is up, the good project has its rules.
    let r = td.get("/api/projects/good/rules").await;
    assert_eq!(r.ok()["source"], "file");
    assert_eq!(
        verbs_of(&r.body),
        ["card.assign", "card.move", "card.label"]
    );
    // The broken project has none, and only the good file has an audit row.
    let r = td.get("/api/projects/broken/rules").await;
    assert_eq!(r.ok()["exists"], false);
    let rows = audit_rows(&td, "rules.file").await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["target"], "good");
    // The other APIs of the broken project work.
    td.get("/api/projects/broken").await.ok();
    // A reload names the broken file at its line.
    let r = td.post_empty("/api/reload").await;
    let errors = r.ok()["errors"].to_string();
    assert!(
        errors.contains("projects/broken.rules.wcl:3:") && errors.contains("a function call"),
        "{errors}"
    );
}

#[tokio::test]
async fn the_export_has_the_api_rules() {
    let td = TestDaemon::start().await;
    let repo = td.dir.path().join("config-terrarium");
    let state = td.daemon.as_ref().unwrap().state.clone();
    state
        .config_repo
        .configure(ConfigRepo::new(repo.clone(), state.state_dir.clone()));
    td.project("demo").await;
    td.put("/api/projects/demo/rules", json!({"text": GOOD}))
        .await
        .ok();
    let r = td
        .post("/api/config/commit", json!({"message": "export"}))
        .await;
    let export: Vec<String> = serde_json::from_value(r.ok()["export"].clone()).unwrap();
    assert!(
        export.contains(&"export/rules/demo.rules.wcl".to_owned()),
        "{export:?}"
    );
    let text = std::fs::read_to_string(repo.join("export/rules/demo.rules.wcl")).unwrap();
    assert_eq!(text, GOOD);
    // The file is committed (the allowlist lets it in).
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["ls-files", "export/rules"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "export/rules/demo.rules.wcl"
    );

    // The rules go: the next export removes the file.
    td.delete("/api/projects/demo/rules").await;
    td.post("/api/config/commit", json!({"message": "export 2"}))
        .await
        .ok();
    assert!(!repo.join("export/rules/demo.rules.wcl").exists());
}
