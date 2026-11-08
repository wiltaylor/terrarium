//! Backups (docs/api.md §32): the API (the operator only, audited) and
//! the offline `terrariumd backup` commands, with a full and a diff round
//! trip through a restore.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::path::Path;
use std::process::Command;

use common::TestDaemon;
use serde_json::{Value, json};

#[tokio::test]
async fn the_api_makes_lists_and_serves_backups_for_the_operator_only() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let sys = td.agent("p", "root", json!({"roles": ["sysadmin"]})).await["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // An agent, also a sysadmin, gets nothing.
    td.post_as(&sys, "/api/backups", json!({}))
        .await
        .err(403, "forbidden");
    td.get_as(&sys, "/api/backups").await.err(403, "forbidden");

    // A diff with no full backup is invalid.
    td.post("/api/backups", json!({"kind": "diff"}))
        .await
        .err(422, "invalid");

    let full = td.post_empty("/api/backups").await;
    assert_eq!(full.status, 201, "{}", full.body);
    let name = full.body["name"].as_str().unwrap().to_owned();
    assert_eq!(full.body["header"]["kind"], "full");
    assert_eq!(full.body["header"]["contains_secrets"], true);
    assert!(td.state.join("backups").join(&name).is_file());

    let diff = td
        .post(
            "/api/backups",
            json!({"kind": "diff", "passphrase": "pass phrase"}),
        )
        .await;
    assert_eq!(diff.status, 201, "{}", diff.body);
    assert_eq!(diff.body["header"]["base"]["name"], name.as_str());
    assert!(diff.body["header"]["encryption"].is_object());
    td.post("/api/backups", json!({"passphrase": ""}))
        .await
        .err(422, "invalid");

    let list = td.get("/api/backups").await;
    let items = list.ok().as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["name"], name.as_str());
    assert_eq!(items[0]["kind"], "full");

    let dl = td.get(&format!("/api/backups/{name}")).await;
    assert_eq!(dl.status, 200);
    assert_eq!(dl.headers["content-type"], "application/zip");
    assert!(dl.body.as_str().unwrap().starts_with("PK"));
    td.get("/api/backups/..%2Fterrarium.db")
        .await
        .err(404, "not_found");
    td.get("/api/backups/nope.zip").await.err(404, "not_found");
    td.get_as(&sys, &format!("/api/backups/{name}"))
        .await
        .err(403, "forbidden");

    // Each create and the download wrote an audit row.
    let audit = td.get("/api/audit?limit=100").await;
    let actions: Vec<String> = audit
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["action"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(
        actions.iter().filter(|a| *a == "backup.create").count(),
        2,
        "{actions:?}"
    );
    assert!(actions.iter().any(|a| a == "backup.download"));
}

fn terrariumd(dir: &Path, state: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_terrariumd"))
        .arg("--socket")
        .arg(dir.join("no-daemon.sock"))
        .arg("--state-dir")
        .arg(state)
        .arg("--config-repo")
        .arg(dir.join("config"))
        .arg("--work-dir")
        .arg(dir.join("work"))
        .args(args)
        .env_remove(terrariumd::backup::ENV_DIR)
        .output()
        .unwrap()
}

fn json_out(out: &std::process::Output) -> Value {
    assert!(out.status.success(), "{out:?}");
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn the_offline_commands_round_trip_a_full_and_a_diff() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let state = d.join("state");
    std::fs::create_dir_all(state.join("credentials")).unwrap();
    std::fs::create_dir_all(d.join("config/prompts")).unwrap();
    std::fs::create_dir_all(d.join("work/p/a")).unwrap();
    std::fs::write(d.join("config/prompts/lead.md"), "v1").unwrap();
    std::fs::write(d.join("work/p/a/notes.md"), "w1").unwrap();
    std::fs::write(state.join("credentials/git"), "token-1").unwrap();
    {
        let c = terrariumd::db::open(&state.join("terrarium.db")).unwrap();
        c.execute(
            "INSERT INTO projects (name, description, created_at, updated_at) VALUES ('alpha', '', '2026-11-03T00:00:00Z', '2026-11-03T00:00:00Z')",
            [],
        )
        .unwrap();
    }

    let full = json_out(&terrariumd(
        d,
        &state,
        &["backup", "create", "--work", "--json"],
    ));
    assert_eq!(full["header"]["roots"], json!(["state", "config", "work"]));
    std::fs::write(d.join("config/prompts/lead.md"), "v2").unwrap();
    let diff = json_out(&terrariumd(
        d,
        &state,
        &["backup", "create", "--kind", "diff", "--work", "--json"],
    ));
    assert_eq!(diff["header"]["base"]["id"], full["header"]["id"]);
    let ls = json_out(&terrariumd(d, &state, &["backup", "ls", "--json"]));
    assert_eq!(ls.as_array().unwrap().len(), 2);
    let diff_path = diff["path"].as_str().unwrap().to_owned();
    let v = json_out(&terrariumd(
        d,
        &state,
        &["backup", "verify", &diff_path, "--json"],
    ));
    assert_eq!(v["kind"], "diff");

    // Lose data, then restore the diff (with its base).
    std::fs::write(d.join("config/prompts/lead.md"), "lost").unwrap();
    std::fs::remove_file(state.join("credentials/git")).unwrap();
    std::fs::remove_file(d.join("work/p/a/notes.md")).unwrap();
    std::fs::remove_file(state.join("terrarium.db")).unwrap();
    let r = json_out(&terrariumd(
        d,
        &state,
        &["backup", "restore", &diff_path, "--json"],
    ));
    assert_eq!(r["base"], full["header"]["id"]);
    assert_eq!(
        std::fs::read_to_string(d.join("config/prompts/lead.md")).unwrap(),
        "v2"
    );
    assert_eq!(
        std::fs::read_to_string(state.join("credentials/git")).unwrap(),
        "token-1"
    );
    assert_eq!(
        std::fs::read_to_string(d.join("work/p/a/notes.md")).unwrap(),
        "w1"
    );
    let c = terrariumd::db::open(&state.join("terrarium.db")).unwrap();
    let n: i64 = c
        .query_row(
            "SELECT count(*) FROM projects WHERE name = 'alpha'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
    // The backups stayed in place.
    assert!(Path::new(&diff_path).is_file());

    // A restore refuses while a daemon answers on the socket.
    let sock = d.join("no-daemon.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let out = terrariumd(d, &state, &["backup", "restore", &diff_path]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("stop it first"));
}
