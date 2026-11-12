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
    td.post_as(
        &sys,
        "/api/backups/retention/repair",
        json!({"confirm":true}),
    )
    .await
    .err(403, "forbidden");
    td.post_as(&sys, "/api/backups", json!({}))
        .await
        .err(403, "forbidden");
    td.get_as(&sys, "/api/backups").await.err(403, "forbidden");
    td.post_as(
        &sys,
        "/api/backups/recovery-release",
        json!({"run":"failed"}),
    )
    .await
    .err(403, "forbidden");

    td.post_as(
        &sys,
        "/api/backups/retention/init",
        json!({"confirm":true,"no_rollback_reference":true}),
    )
    .await
    .err(403, "forbidden");

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

    // The restore held the state lock and left the lock file in place.
    assert!(state.join(terrariumd::STATE_LOCK).is_file());

    // A restore refuses while a daemon on another socket holds the state
    // lock.
    {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(state.join(terrariumd::STATE_LOCK))
            .unwrap();
        let _held =
            nix::fcntl::Flock::lock(f, nix::fcntl::FlockArg::LockExclusiveNonblock).unwrap();
        let out = terrariumd(d, &state, &["backup", "restore", &diff_path]);
        assert!(!out.status.success());
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("is locked") && err.contains("stop it first"),
            "{err}"
        );
    }

    // A restore refuses while a daemon answers on the socket.
    let sock = d.join("no-daemon.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let out = terrariumd(d, &state, &["backup", "restore", &diff_path]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("stop it first"));
}

/// The scheduled runs (docs/api.md §32.4): the key from the secret, a
/// failure alert once per streak, a verify of each backup, a rotated key
/// that starts a new full backup, a prune only after a verified full
/// backup, a missed run, and a restore drill of the newest backup. No key
/// is in an event, an audit row, or a mail.
#[tokio::test]
async fn scheduled_runs_seal_verify_rotate_prune_and_alert() {
    use std::time::Duration;
    use terrariumd::backup::schedule::{Config, Runner};
    use terrariumd::backup::{self, Kind};

    const KEY1: &str = "first backup pass phrase 1";
    const KEY2: &str = "second backup pass phrase 2";
    let td = TestDaemon::start().await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    let dir = backup::default_dir(&td.state);
    let mut runner = Runner::new(Config {
        interval: Some(Duration::from_secs(86_400)),
        keep: 2,
        ..Config::default()
    });

    // No key: the run fails, makes no backup, and mails once per streak.
    let r = runner.run(&state).await;
    assert!(r.error.as_deref().unwrap().contains("backup-key"), "{r:?}");
    assert!(r.mailed);
    let r = runner.run(&state).await;
    assert!(r.error.is_some() && !r.mailed, "{r:?}");
    assert!(backup::list(&dir).unwrap().is_empty());

    td.put(
        "/api/secrets/global/backup-key",
        json!({"kind": "api_key", "fields": {"value": KEY1}}),
    )
    .await
    .ok();
    let f1 = runner.run(&state).await;
    assert!(f1.error.is_none() && f1.verified, "{f1:?}");
    assert_eq!(f1.kind, Kind::Full);
    let d1 = runner.run(&state).await;
    assert_eq!(d1.kind, Kind::Diff, "{d1:?}");
    let d1_path = dir.join(d1.name.as_ref().unwrap());
    let h = backup::read_header(&d1_path).unwrap();
    assert!(h.encryption.is_some());

    // Rotate the key: the next run is a full backup with the new key.
    td.put(
        "/api/secrets/global/backup-key",
        json!({"kind": "api_key", "fields": {"value": KEY2}}),
    )
    .await
    .ok();
    let f2 = runner.run(&state).await;
    assert_eq!(f2.kind, Kind::Full, "{f2:?}");
    assert!(f2.removed.is_empty());
    let d2 = runner.run(&state).await;
    assert_eq!(d2.kind, Kind::Diff, "{d2:?}");
    // The old backups stay restorable with the previous key.
    backup::verify(&d1_path, None, Some(KEY1)).unwrap();
    assert!(backup::verify(&d1_path, None, Some(KEY2)).is_err());

    // No updater is reachable in this fixture: verification never deletes.
    runner.cfg.full_every = Duration::ZERO;
    let f3 = runner.run(&state).await;
    assert_eq!(f3.kind, Kind::Full, "{f3:?}");
    assert!(f3.removed.is_empty());
    let names: Vec<String> = backup::list(&dir)
        .unwrap()
        .into_iter()
        .map(|l| l.name)
        .collect();
    assert_eq!(names.len(), 5, "{names:?}");

    // A missed run mails once per due time.
    let later = chrono::Utc::now() + chrono::Duration::days(3);
    assert!(runner.check_missed(&state, later, None).await);
    assert!(!runner.check_missed(&state, later, None).await);
    // A run that was tried is not missed.
    let mut fresh = Runner::new(runner.cfg.clone());
    assert!(!fresh.check_missed(&state, later, Some(later)).await);

    // The restore drill of the newest diff: the restored database has
    // the key secret, and the live data does not change.
    let d2_path = dir.join(d2.name.as_ref().unwrap());
    let scratch = dir.join(format!("{}t", backup::DRILL_PREFIX));
    let drilled = backup::drill(&d2_path, None, Some(KEY2), &scratch).unwrap();
    assert_eq!(drilled.integrity, "ok");
    assert!(drilled.secrets_key);
    let c = rusqlite::Connection::open(scratch.join("state/terrarium.db")).unwrap();
    let n: i64 = c
        .query_row(
            "SELECT count(*) FROM secrets WHERE scope = 'global' AND name = 'backup-key'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
    std::fs::remove_dir_all(&scratch).unwrap();

    // Each run is in the audit and the event log; no key is anywhere.
    let (actions, events, mails) = state
        .db
        .read(|c| {
            let col = |sql: &str| -> Vec<String> {
                let mut s = c.prepare(sql).unwrap();
                s.query_map([], |r| r.get::<_, String>(0))
                    .unwrap()
                    .map(Result::unwrap)
                    .collect()
            };
            Ok((
                col("SELECT action || ' ' || detail FROM audit WHERE actor = 'system:backup'"),
                col("SELECT summary || ' ' || payload FROM events WHERE kind = 'backup'"),
                col("SELECT coalesce(subject, '') || ' ' || body FROM messages WHERE to_kind = 'operator' AND subject LIKE 'Scheduled backup%'"),
            ))
        })
        .await
        .unwrap();
    let count = |p: &str| actions.iter().filter(|a| a.starts_with(p)).count();
    assert_eq!(count("backup.failed "), 2, "{actions:?}");
    assert_eq!(count("backup.recovered "), 1, "{actions:?}");
    assert_eq!(count("backup.verify "), 5, "{actions:?}");
    assert_eq!(count("backup.prune "), 0, "{actions:?}");
    assert_eq!(count("backup.missed "), 1, "{actions:?}");
    assert!(
        events.iter().any(|e| e.contains("\"change\":\"verified\"")),
        "{events:?}"
    );
    assert!(
        events.iter().any(|e| e.contains("\"change\":\"failed\"")),
        "{events:?}"
    );
    assert_eq!(mails.len(), 2, "{mails:?}");
    for text in actions.iter().chain(&events).chain(&mails) {
        assert!(
            !text.contains(KEY1) && !text.contains(KEY2),
            "a key leaked: {text}"
        );
    }
}

/// A scheduled backup that does not verify goes to `.unverified-<name>`:
/// it is no base and no kept backup, nothing is pruned, and the streak
/// mails once. A schedule turned on over an old manual backup mails no
/// missed run at once, and a missed run mails once, also over a restart
/// (the marks in the backup dir).
#[tokio::test]
async fn a_scheduled_backup_that_does_not_verify_is_set_aside_and_missed_mails_once() {
    use std::time::Duration;
    use terrariumd::backup::schedule::{Config, MARKS_FILE, Runner, UNVERIFIED_PREFIX};
    use terrariumd::backup::{self, Kind};

    const KEY: &str = "verify check pass phrase 1";
    let td = TestDaemon::start().await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    let dir = backup::default_dir(&td.state);
    td.put(
        "/api/secrets/global/backup-key",
        json!({"kind": "api_key", "fields": {"value": KEY}}),
    )
    .await
    .ok();
    let manual = td.post_empty("/api/backups").await;
    assert_eq!(manual.status, 201, "{}", manual.body);
    let manual = manual.body["name"].as_str().unwrap().to_owned();

    // Turned on over the manual backup: the first check marks the
    // schedule on and finds nothing, even days later.
    let cfg = Config {
        interval: Some(Duration::from_secs(86_400)),
        keep: 1,
        full_every: Duration::ZERO,
        ..Config::default()
    };
    let now = chrono::Utc::now();
    let mut runner = Runner::new(cfg.clone());
    assert!(
        !runner
            .check_missed(&state, now + chrono::Duration::days(3), None)
            .await
    );
    assert!(dir.join(MARKS_FILE).is_file());
    let late = now + chrono::Duration::days(5);
    assert!(runner.check_missed(&state, late, None).await);
    // A restart: a new runner reads the marks and does not mail again.
    let mut restarted = Runner::new(cfg);
    assert!(!restarted.check_missed(&state, late, None).await);

    let f1 = runner.run(&state).await;
    assert!(f1.error.is_none() && f1.kind == Kind::Full, "{f1:?}");
    assert!(f1.removed.is_empty());
    let f1 = f1.name.unwrap();

    // The verify fails: the file goes aside, f1 stays, and one mail.
    runner.verify = |_, _, _| Err(anyhow::anyhow!("a bad file in the test"));
    for first in [true, false] {
        let r = runner.run(&state).await;
        let e = r.error.as_deref().unwrap();
        assert!(e.contains("does not verify (moved to .unverified-"), "{e}");
        assert_eq!(r.mailed, first, "{r:?}");
        assert!(r.removed.is_empty() && !r.verified, "{r:?}");
        let names: Vec<String> = backup::list(&dir)
            .unwrap()
            .into_iter()
            .map(|l| l.name)
            .collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&f1) && names.contains(&manual));
    }
    let aside = std::fs::read_dir(&dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(UNVERIFIED_PREFIX)
        })
        .count();
    assert_eq!(aside, 2);

    // A good run again: the newest full is f1's successor, f1 goes.
    runner.verify = backup::verify;
    let f2 = runner.run(&state).await;
    assert!(f2.error.is_none(), "{f2:?}");
    assert!(f2.removed.is_empty());

    let mails = state
        .db
        .read(|c| {
            let mut s = c
                .prepare(
                    "SELECT subject FROM messages WHERE to_kind = 'operator' \
                     AND subject LIKE 'Scheduled backup%' ORDER BY subject",
                )
                .unwrap();
            Ok(s.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>())
        })
        .await
        .unwrap();
    assert_eq!(
        mails,
        vec!["Scheduled backup failed", "Scheduled backup missed"],
        "{mails:?}"
    );
}

/// A marks file that cannot be written (here a dir with content has its
/// name): the runner keeps the marks in memory, so a failed run is not
/// tried again at once, and the missed check still mails once.
#[tokio::test]
async fn a_marks_file_that_cannot_be_written_runs_no_loop_and_keeps_the_missed_check() {
    use std::time::Duration;
    use terrariumd::backup;
    use terrariumd::backup::schedule::{Config, MARKS_FILE, Runner};

    let td = TestDaemon::start().await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    let dir = backup::default_dir(&td.state);
    std::fs::create_dir_all(dir.join(MARKS_FILE).join("x")).unwrap();
    let mut runner = Runner::new(Config {
        interval: Some(Duration::from_secs(86_400)),
        ..Config::default()
    });
    assert!(!runner.check_missed(&state, chrono::Utc::now(), None).await);
    // No key: the run fails and makes no backup.
    let r = runner.run(&state).await;
    assert!(r.error.is_some() && r.name.is_none(), "{r:?}");
    let next = runner.next_at(&state).await.unwrap();
    assert!(
        next > chrono::Utc::now() + chrono::Duration::hours(23),
        "the next run is at once: {next}"
    );
    let late = chrono::Utc::now() + chrono::Duration::days(3);
    assert!(runner.check_missed(&state, late, None).await);
    assert!(!runner.check_missed(&state, late, None).await);
    assert!(dir.join(MARKS_FILE).is_dir());
}

/// `terrariumd backup drill`: a sealed backup restores into a scratch dir
/// that goes at the end; a dir with content is refused and kept.
#[test]
fn the_drill_command_restores_into_a_scratch_dir_and_removes_it() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let state = d.join("state");
    std::fs::create_dir_all(state.join("credentials")).unwrap();
    std::fs::create_dir_all(d.join("config")).unwrap();
    std::fs::write(state.join("credentials/git"), "token-1").unwrap();
    drop(terrariumd::db::open(&state.join("terrarium.db")).unwrap());
    let pass = d.join("pass");
    std::fs::write(&pass, "drill pass phrase\n").unwrap();
    let pass = pass.to_str().unwrap();
    let full = json_out(&terrariumd(
        d,
        &state,
        &["backup", "create", "--passphrase-file", pass, "--json"],
    ));
    let file = full["path"].as_str().unwrap().to_owned();
    let r = json_out(&terrariumd(
        d,
        &state,
        &[
            "backup",
            "drill",
            &file,
            "--passphrase-file",
            pass,
            "--json",
        ],
    ));
    assert_eq!(r["integrity"], "ok");
    assert_eq!(r["id"], full["header"]["id"]);
    assert!(r["files"].as_u64().unwrap() > 0);
    assert!(!Path::new(r["dir"].as_str().unwrap()).exists());
    // The live data did not change.
    assert_eq!(
        std::fs::read_to_string(state.join("credentials/git")).unwrap(),
        "token-1"
    );

    let busy = d.join("busy");
    std::fs::create_dir_all(&busy).unwrap();
    std::fs::write(busy.join("mine"), "x").unwrap();
    let out = terrariumd(
        d,
        &state,
        &[
            "backup",
            "drill",
            &file,
            "--passphrase-file",
            pass,
            "--into",
            busy.to_str().unwrap(),
        ],
    );
    assert!(!out.status.success());
    assert!(busy.join("mine").is_file());
    // A wrong passphrase fails.
    let bad = d.join("bad");
    std::fs::write(&bad, "wrong\n").unwrap();
    let out = terrariumd(
        d,
        &state,
        &[
            "backup",
            "drill",
            &file,
            "--passphrase-file",
            bad.to_str().unwrap(),
        ],
    );
    assert!(!out.status.success());
}
