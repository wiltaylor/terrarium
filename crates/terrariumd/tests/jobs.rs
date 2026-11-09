//! The job queue of a project (TER-71): a real `terrarium_host::Client`
//! runs the jobs on this machine, against a bare git repo as the remote.
//! The recipe is a stand-in (`test -f OK`), as the lead asked: the real
//! one (`just ci::validate`) comes with TER-69.
//!
//! The tests of the card flow run twice ([`both_flows!`], TER-170): with
//! `board_flow = code` (the daemon's code) and with `board_flow = rules`
//! (the built-in rule pack). They check the same card states, labels,
//! comments, and mails; the rule engine acts a moment after the event, so
//! each check polls. The lab board has a `validating` column, so in rules
//! mode project `p` adds the opt-in validate rules
//! (`rules_file::VALIDATE_RULES`) to the pack. [`the_two_flows_leave_the_same_board`] compares the
//! two boards of one scenario field by field.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::TestDaemon;
use serde_json::{Value, json};
use terrarium_core::protocol::HostCapabilities;
use terrarium_host::{Client, ClientOptions};

/// How long a poll waits before the test fails. A bound for a hung test
/// only: each poll returns as soon as its state is there.
const DEADLINE: Duration = Duration::from_secs(300);

/// Run the test `$name(flow)` once with each board flow (TER-170): the
/// tests `$name::code` and `$name::rules`.
macro_rules! both_flows {
    ($name:ident) => {
        mod $name {
            #[tokio::test]
            async fn code() {
                super::$name("code").await;
            }
            #[tokio::test]
            async fn rules() {
                super::$name("rules").await;
            }
        }
    };
}

#[tokio::test]
async fn host_caps_accept_names_ids_and_ssh_aliases() {
    let td = TestDaemon::start().await;
    let h = td
        .post("/api/hosts", json!({"name": "machine"}))
        .await
        .ok()
        .clone();
    let id = h["id"].as_str().unwrap();
    // Legacy rows were stored under names, and aliases may have distinct caps.
    let legacy_id = id.to_owned();
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .write("operator", move |t| {
            t.execute(
                "INSERT INTO job_hosts VALUES ('machine', 1, 'operator', 'old')",
                [],
            )?;
            t.execute(
                "INSERT INTO job_hosts VALUES (?1, 3, 'operator', 'new')",
                [&legacy_id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let legacy = td.get("/api/job-hosts").await.ok().clone();
    assert_eq!(legacy["hosts"].as_array().unwrap().len(), 1);
    assert_eq!(legacy["hosts"][0]["host"], id);
    assert_eq!(legacy["hosts"][0]["heavy_jobs"], 1);
    let cap = td
        .put("/api/job-hosts/machine", json!({"heavy_jobs": 2}))
        .await
        .ok()
        .clone();
    assert_eq!(cap["hosts"][0]["host"], id);
    let cap = td
        .put(&format!("/api/job-hosts/{id}"), json!({"heavy_jobs": 1}))
        .await
        .ok()
        .clone();
    assert_eq!(cap["hosts"].as_array().unwrap().len(), 1);
    assert_eq!(cap["hosts"][0]["heavy_jobs"], 1);
    let rows: i64 = td
        .daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .read(|c| Ok(c.query_row("SELECT count(*) FROM job_hosts", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(rows, 1, "a cap write must remove legacy aliases");
    td.delete(&format!("/api/job-hosts/{id}")).await.ok();
    let cap = td
        .put("/api/job-hosts/localhost", json!({"heavy_jobs": 2}))
        .await
        .ok()
        .clone();
    let key = cap["hosts"][0]["host"].as_str().unwrap();
    assert!(key.parse::<std::net::IpAddr>().is_ok());
    let cap = td
        .put(&format!("/api/job-hosts/{key}"), json!({"heavy_jobs": 1}))
        .await
        .ok()
        .clone();
    assert_eq!(cap["hosts"].as_array().unwrap().len(), 1);
    assert_eq!(cap["hosts"][0]["host"], key);
    assert_eq!(cap["hosts"][0]["heavy_jobs"], 1);
}

#[tokio::test]
async fn host_counts_respect_the_readers_project() {
    use terrariumd::store::jobs::{self, JobKind, NewJob};
    let td = TestDaemon::start().await;
    for project in ["p", "q"] {
        td.project(project).await;
        let card = td
            .post(
                &format!("/api/projects/{project}/cards"),
                json!({"title": "job"}),
            )
            .await
            .ok()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let project = project.to_owned();
        td.daemon
            .as_ref()
            .unwrap()
            .state
            .db
            .write("operator", move |t| {
                let j = jobs::create(
                    t,
                    NewJob {
                        project,
                        card,
                        kind: JobKind::Validate,
                        batch: Vec::new(),
                        planned_secs: None,
                        repo: "app".into(),
                        branch: "next".into(),
                        commit: None,
                        by_agent: None,
                    },
                )?;
                jobs::start(t, &j.id, "machine", Some(1))?;
                jobs::set_host_cap(t, "machine", Some(2))?;
                Ok(())
            })
            .await
            .unwrap();
    }
    for (name, roles, all) in [
        ("worker", json!([]), false),
        ("lead", json!(["project_admin"]), false),
        ("architect", json!(["architect"]), true),
        ("sysadmin", json!(["sysadmin"]), true),
    ] {
        td.agent("p", name, json!({"roles": roles})).await;
        let v = td
            .get_as(&format!("p/{name}"), "/api/job-hosts")
            .await
            .ok()
            .clone();
        let h = &v["hosts"][0];
        assert_eq!(h["running"], 2);
        assert_eq!(h["projects"]["p"], 1);
        assert_eq!(h["projects"].get("q").is_some(), all);
        assert_eq!(h.get("updated_by").is_some(), all);
        assert_eq!(h.get("updated_at").is_some(), all);
    }
    let v = td.get("/api/job-hosts").await.ok().clone();
    assert_eq!(v["hosts"][0]["projects"], json!({"p": 1, "q": 1}));
    assert!(v["hosts"][0].get("updated_by").is_some());
    td.project("empty").await;
    td.agent("empty", "worker", json!({})).await;
    let v = td
        .get_as("empty/worker", "/api/job-hosts")
        .await
        .ok()
        .clone();
    assert_eq!(v["hosts"][0]["projects"], json!({"empty": 0}));
}

/// TER-105: an SSH target on an address of a host agent's machine counts
/// on the host agent: `job-hosts` shows one host, and its cap holds across
/// the host-agent target and the SSH target.
/// TER-158 (D4): each card of the board and of `GET /api/cards/{id}` has
/// its open job (kind, state, and for a queued job its place n of m) and
/// the status of its assignee agent, so the tile does not read labels.
#[tokio::test]
async fn cards_carry_their_job_state_and_assignee_status() {
    use terrariumd::store::jobs::{self, JobKind, NewJob};
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.agent("p", "w", json!({})).await;
    let mut ids = Vec::new();
    for title in ["running", "first", "second", "merge", "none"] {
        let id = td
            .post("/api/projects/p/cards", json!({"title": title}))
            .await
            .ok()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        ids.push(id);
    }
    td.post_empty_as("p/w", &format!("/api/cards/{}/claim", ids[4]))
        .await
        .ok();
    let cards = ids.clone();
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .write("operator", move |t| {
            for (i, card) in cards.iter().take(4).enumerate() {
                let j = jobs::create(
                    t,
                    NewJob {
                        project: "p".into(),
                        card: card.clone(),
                        kind: if i == 3 {
                            JobKind::Merge
                        } else {
                            JobKind::Validate
                        },
                        batch: Vec::new(),
                        planned_secs: None,
                        repo: "app".into(),
                        branch: format!("b{i}"),
                        commit: None,
                        by_agent: None,
                    },
                )?;
                if i == 0 {
                    jobs::start(t, &j.id, "machine", Some(1))?;
                }
            }
            Ok(())
        })
        .await
        .unwrap();

    let board = td.get("/api/projects/p/board").await.ok().clone();
    let card = |id: &str| {
        board["cards"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == id)
            .unwrap()
            .clone()
    };
    let running = card(&ids[0]);
    assert_eq!(running["job"]["state"], "running", "{running:#?}");
    assert_eq!(running["job"]["kind"], "validate");
    assert!(running["job"].get("place").is_none());
    assert!(running["job"].get("queued").is_none());
    // Jobs queued in the same millisecond keep the queue order of their
    // IDs, so the two validations are 1 and 2 of 2 in some order. The
    // merge has its own lane (TER-159), so it is 1 of 1.
    let mut places = Vec::new();
    for id in &ids[1..=2] {
        let c = card(id);
        assert_eq!(c["job"]["state"], "queued", "{c:#?}");
        assert_eq!(c["job"]["queued"], 2);
        places.push(c["job"]["place"].as_i64().unwrap());
    }
    places.sort_unstable();
    assert_eq!(places, [1, 2]);
    let merge = card(&ids[3]);
    assert_eq!(merge["job"]["kind"], "merge");
    assert_eq!(merge["job"]["state"], "queued", "{merge:#?}");
    assert_eq!(merge["job"]["place"], 1);
    assert_eq!(merge["job"]["queued"], 1);
    let none = card(&ids[4]);
    assert!(none.get("job").is_none(), "{none:#?}");
    let w = td.get("/api/agents/p%2Fw").await.ok().clone();
    assert_eq!(none["assignee_status"], w["status"]);
    assert!(card(&ids[0]).get("assignee_status").is_none());

    // One card has the same fields.
    let one = td.get(&format!("/api/cards/{}", ids[1])).await.ok().clone();
    assert_eq!(one["job"], card(&ids[1])["job"]);
    let one = td.get(&format!("/api/cards/{}", ids[4])).await.ok().clone();
    assert_eq!(one["assignee_status"], w["status"]);
}

/// The seconds from `from` to `to` (RFC 3339).
fn secs_between(from: &Value, to: &Value) -> i64 {
    let t = |v: &Value| chrono::DateTime::parse_from_rfc3339(v.as_str().unwrap()).unwrap();
    (t(to) - t(from)).num_seconds()
}

/// TER-155 D2: the ETA of a job starts when the job runs. A queued job's
/// card has no ETA; the start sets `eta_at` = start + planned duration; a
/// rerun clears it and its start sets it again; a job with no duration
/// sets no ETA. A duration given for a card with an open job is the job's
/// planned duration.
#[tokio::test]
async fn a_jobs_eta_starts_when_the_job_runs() {
    use terrariumd::store::jobs::{self, JobEnd, JobKind, NewJob};
    let td = TestDaemon::start().await;
    td.project("p").await;
    let w = td.agent("p", "w", json!({})).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let label = format!("agent:{w}");
    let mut ids = Vec::new();
    for title in ["planned", "none", "later"] {
        let id = td
            .post("/api/projects/p/cards", json!({"title": title}))
            .await
            .ok()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        td.post_empty_as("p/w", &format!("/api/cards/{id}/claim"))
            .await
            .ok();
        td.patch_as("p/w", &format!("/api/cards/{id}"), json!({"eta_at": "3h"}))
            .await
            .ok();
        ids.push(id);
    }
    let db = td.daemon.as_ref().unwrap().state.db.clone();
    let create = |card: String, planned_secs: Option<i64>| {
        let (db, label) = (db.clone(), label.clone());
        async move {
            db.write(&label, move |t| {
                jobs::create(
                    t,
                    NewJob {
                        project: "p".into(),
                        card,
                        kind: JobKind::Validate,
                        repo: "app".into(),
                        branch: "b".into(),
                        commit: None,
                        by_agent: Some("w".into()),
                        batch: Vec::new(),
                        planned_secs,
                    },
                )
            })
            .await
            .unwrap()
        }
    };
    let card = |id: String| {
        let td = &td;
        async move { td.get(&format!("/api/cards/{id}")).await.ok().clone() }
    };

    // Queued with a planned duration: the old ETA goes, none until the start.
    let j = create(ids[0].clone(), Some(1200)).await;
    assert_eq!(j.planned_secs, Some(1200));
    let c = card(ids[0].clone()).await;
    assert!(c.get("eta_at").is_none(), "{c:#}");
    assert_eq!(c["job"]["planned_secs"], 1200, "{c:#}");
    // The start: start + 20m, set by the actor that queued it.
    let jid = j.id.clone();
    let started = db
        .write("system", move |t| {
            assert!(jobs::start(t, &jid, "machine", Some(1))?);
            jobs::get(t, &jid)
        })
        .await
        .unwrap();
    let c = card(ids[0].clone()).await;
    let start = json!(started.started_at.clone().unwrap());
    assert_eq!(secs_between(&start, &c["eta_at"]), 1200, "{c:#}");
    assert_eq!(c["eta_set_by_name"], "p/w", "{c:#}");
    let ev = td
        .get(&format!("/api/cards/{}/events", ids[0]))
        .await
        .ok()
        .clone();
    assert!(
        ev.as_array()
            .unwrap()
            .iter()
            .any(|e| e["data"]["change"] == "eta" && e["data"]["job"] == true),
        "{ev:#}"
    );
    // A failure and its rerun: the rerun keeps the duration, the queue
    // clears the ETA, and the rerun's start sets it again.
    let jid = j.id.clone();
    let rerun = db
        .write("system", move |t| {
            let failed = jobs::finish(t, &jid, &JobEnd::default())?.unwrap();
            jobs::rerun(t, &failed, None)
        })
        .await
        .unwrap();
    assert_eq!(rerun.planned_secs, Some(1200));
    let c = card(ids[0].clone()).await;
    assert!(c.get("eta_at").is_none(), "the rerun waits: {c:#}");
    tokio::time::sleep(Duration::from_millis(20)).await;
    let rid = rerun.id.clone();
    let restarted = db
        .write("system", move |t| {
            assert!(jobs::start(t, &rid, "machine", Some(1))?);
            jobs::get(t, &rid)
        })
        .await
        .unwrap();
    let c = card(ids[0].clone()).await;
    let restart = json!(restarted.started_at.clone().unwrap());
    assert_eq!(secs_between(&restart, &c["eta_at"]), 1200, "{c:#}");
    assert!(restarted.started_at > started.started_at);

    // Requeue keeps the plan, clears the old ETA, and restarts its clock.
    let rid = rerun.id.clone();
    let queued = db
        .write("system", move |t| jobs::requeue(t, &rid, "unreachable"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(queued.planned_secs, Some(1200));
    assert!(queued.started_at.is_none());
    assert!(card(ids[0].clone()).await.get("eta_at").is_none());
    let rid = rerun.id.clone();
    let restarted = db
        .write("system", move |t| {
            assert!(jobs::start(t, &rid, "machine", Some(1))?);
            jobs::get(t, &rid)
        })
        .await
        .unwrap();
    let c = card(ids[0].clone()).await;
    assert_eq!(
        secs_between(&json!(restarted.started_at.unwrap()), &c["eta_at"]),
        1200
    );

    // Cancelling a running planned job must not leave an overdue ETA.
    let rid = rerun.id.clone();
    db.write("system", move |t| jobs::cancel(t, &rid))
        .await
        .unwrap();
    assert!(card(ids[0].clone()).await.get("eta_at").is_none());

    // No duration: the job leaves the card's ETA alone and sets none.
    let j = create(ids[1].clone(), None).await;
    let before = card(ids[1].clone()).await["eta_at"].clone();
    assert!(before.is_string());
    let jid = j.id.clone();
    db.write("system", move |t| jobs::start(t, &jid, "machine", Some(1)))
        .await
        .unwrap();
    let c = card(ids[1].clone()).await;
    assert_eq!(c["eta_at"], before, "{c:#}");
    assert!(c["job"].get("planned_secs").is_none(), "{c:#}");

    // D2: a move to validating with a queued validate job sets its planned
    // duration, not a point-in-time card ETA. No ETA until the job starts.
    let j = create(ids[2].clone(), None).await;
    let path = format!("/api/cards/{}", ids[2]);
    let c = td
        .patch_as(
            "p/w",
            &path,
            json!({"column": "validating", "eta_at": "45m"}),
        )
        .await
        .ok()
        .clone();
    assert!(c.get("eta_at").is_none(), "{c:#}");
    assert_eq!(c["job"]["planned_secs"], 2700, "{c:#}");
    assert_eq!(c["column"], "validating", "{c:#}");
    assert_eq!(c["job"]["state"], "queued", "{c:#}");
    let jid = j.id.clone();
    let queued = db.read(move |c| jobs::get(c, &jid)).await.unwrap();
    assert_eq!(queued.planned_secs, Some(2700));
    assert!(queued.started_at.is_none());
    let jid = j.id.clone();
    let started = db
        .write("system", move |t| {
            jobs::start(t, &jid, "machine", Some(1))?;
            jobs::get(t, &jid)
        })
        .await
        .unwrap();
    let c = card(ids[2].clone()).await;
    let start = json!(started.started_at.clone().unwrap());
    assert_eq!(secs_between(&start, &c["eta_at"]), 2700, "{c:#}");
    // While it runs, a new duration counts from the start.
    let c = td
        .patch_as("p/w", &path, json!({"eta_at": "1h"}))
        .await
        .ok()
        .clone();
    assert_eq!(secs_between(&start, &c["eta_at"]), 3600, "{c:#}");
    let later = (chrono::Utc::now() + chrono::Duration::hours(5)).to_rfc3339();
    let c = td
        .patch_as("p/w", &path, json!({"eta_at": later}))
        .await
        .ok()
        .clone();
    assert!(secs_between(&start, &c["eta_at"]) > 4 * 3600, "{c:#}");
    // Another member may not set the job's duration.
    td.agent("p", "w2", json!({})).await;
    td.patch_as("p/w2", &path, json!({"eta_at": "2h"}))
        .await
        .err(403, "forbidden");

    // Both halves inherit a batch's duration, wait without an ETA, and
    // set their cards' ETA from their own start.
    let jid = j.id.clone();
    let batch = [ids[0].clone(), ids[2].clone()]
        .into_iter()
        .map(|card| jobs::BatchCard {
            card,
            branch: "b".into(),
            by_agent: Some("w".into()),
            outcome: None,
            note: None,
        })
        .collect::<Vec<_>>();
    let halves = db
        .write("system", move |t| {
            jobs::cancel(t, &jid)?;
            let parent = jobs::create(
                t,
                NewJob {
                    project: "p".into(),
                    card: batch[0].card.clone(),
                    kind: JobKind::Merge,
                    repo: "app".into(),
                    branch: "b".into(),
                    commit: None,
                    by_agent: Some("w".into()),
                    batch: batch.clone(),
                    planned_secs: Some(1200),
                },
            )?;
            jobs::start(t, &parent.id, "machine", Some(1))?;
            let red = jobs::finish(t, &parent.id, &JobEnd::default())?.unwrap();
            jobs::split(t, &red, &batch)
        })
        .await
        .unwrap();
    for half in [halves.0, halves.1] {
        assert_eq!(half.planned_secs, Some(1200));
        assert!(card(half.card.clone()).await.get("eta_at").is_none());
        let hid = half.id.clone();
        let started = db
            .write("system", move |t| {
                assert!(jobs::start(t, &hid, "machine", Some(1))?);
                jobs::get(t, &hid)
            })
            .await
            .unwrap();
        let c = card(half.card.clone()).await;
        assert_eq!(
            secs_between(&json!(started.started_at.unwrap()), &c["eta_at"]),
            1200
        );
    }
}

#[tokio::test]
async fn an_ssh_alias_of_a_host_agent_is_one_host() {
    use terrariumd::store::jobs::{self, JobKind, NewJob};
    let td = TestDaemon::start().await;
    let id = td.post("/api/hosts", json!({"name": "helios"})).await.ok()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    td.post("/api/hosts", json!({"name": "other"})).await.ok();
    for (project, target) in [
        (
            "p",
            json!({"name": "p-build", "kind": "host", "host": "helios", "project": "p"}),
        ),
        (
            "q",
            json!({"name": "q-vm", "kind": "ssh", "address": "ci@10.3.0.2:22", "project": "q"}),
        ),
    ] {
        td.project(project).await;
        td.post("/api/targets", target.clone()).await.ok();
        td.put(
            &format!("/api/projects/{project}/delivery"),
            json!({"target": target["name"], "recipe": "true"}),
        )
        .await
        .ok();
    }
    let db = td.daemon.as_ref().unwrap().state.db.clone();
    // A job of q runs on the SSH target, stored under its address. No job
    // is queued, so the daemon's own tick starts nothing during the test.
    let card = td
        .post("/api/projects/q/cards", json!({"title": "job"}))
        .await
        .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    db.write("operator", move |t| {
        let j = jobs::create(
            t,
            NewJob {
                project: "q".into(),
                card,
                kind: JobKind::Validate,
                batch: Vec::new(),
                planned_secs: None,
                repo: "app".into(),
                branch: "next".into(),
                commit: None,
                by_agent: None,
            },
        )?;
        jobs::start(t, &j.id, "10.3.0.2", Some(1))?;
        Ok(())
    })
    .await
    .unwrap();
    td.put("/api/job-hosts/helios", json!({"heavy_jobs": 1}))
        .await
        .ok();
    // The host of the queue of p, its running jobs, and its cap: what the
    // pick checks before a job of p starts.
    let room = |db: terrariumd::db::Db, project: &'static str| async move {
        db.read(move |c| jobs::host_room(c, project)).await.unwrap()
    };
    // Without an alias: two hosts, and helios has room for a job of p.
    let v = td.get("/api/job-hosts").await.ok().clone();
    assert_eq!(v["hosts"].as_array().unwrap().len(), 2, "{v}");
    assert_eq!(room(db.clone(), "p").await, Some((id.clone(), 0, 1)));
    // 10.3.0.2 has no cap of its own.
    assert_eq!(room(db.clone(), "q").await, None);

    // Only a sysadmin or the operator sets aliases.
    td.agent("q", "worker", json!({})).await;
    td.put_as(
        "q/worker",
        "/api/job-hosts/helios/aliases",
        json!({"aliases": ["10.3.0.2"]}),
    )
    .await
    .err(403, "forbidden");
    td.put(
        "/api/job-hosts/nope/aliases",
        json!({"aliases": ["10.3.0.2"]}),
    )
    .await
    .err(404, "not_found");
    td.put(
        "/api/job-hosts/helios/aliases",
        json!({"aliases": ["other"]}),
    )
    .await
    .err(422, "invalid");

    let v = td
        .put(
            "/api/job-hosts/helios/aliases",
            json!({"aliases": ["ci@10.3.0.2:22", "Helios.LAN"]}),
        )
        .await
        .ok()
        .clone();
    let hosts = v["hosts"].as_array().unwrap();
    assert_eq!(hosts.len(), 1, "{v}");
    assert_eq!(hosts[0]["host"], id);
    assert_eq!(hosts[0]["name"], "helios");
    assert_eq!(hosts[0]["aliases"], json!(["10.3.0.2", "helios.lan"]));
    assert_eq!(hosts[0]["running"], 1);
    assert_eq!(hosts[0]["projects"]["q"], 1);
    assert_eq!(hosts[0]["heavy_jobs"], 1);
    // The cap holds across both targets: the q job on the SSH target
    // fills helios, so the p job waits.
    assert_eq!(room(db.clone(), "p").await, Some((id.clone(), 1, 1)));
    assert_eq!(room(db.clone(), "q").await, Some((id.clone(), 1, 1)));
    // A worker sees the name, not the aliases.
    let v = td.get_as("q/worker", "/api/job-hosts").await.ok().clone();
    assert_eq!(v["hosts"][0]["name"], "helios");
    assert!(v["hosts"][0].get("aliases").is_none());

    // One host agent per alias.
    td.put(
        "/api/job-hosts/other/aliases",
        json!({"aliases": ["10.3.0.2"]}),
    )
    .await
    .err(409, "conflict");
    // A cap set by the alias lands on the host agent.
    let v = td
        .put("/api/job-hosts/10.3.0.2", json!({"heavy_jobs": 2}))
        .await
        .ok()
        .clone();
    assert_eq!(v["hosts"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v["hosts"][0]["host"], id);
    assert_eq!(v["hosts"][0]["heavy_jobs"], 2);
    assert_eq!(room(db.clone(), "p").await, Some((id.clone(), 1, 2)));
    td.put("/api/job-hosts/helios", json!({"heavy_jobs": 1}))
        .await
        .ok();

    // An empty list takes the aliases away, and so does a revoke.
    td.put("/api/job-hosts/helios/aliases", json!({"aliases": []}))
        .await
        .ok();
    assert_eq!(room(db.clone(), "p").await, Some((id.clone(), 0, 1)));
    td.put(
        "/api/job-hosts/helios/aliases",
        json!({"aliases": ["10.3.0.2"]}),
    )
    .await
    .ok();
    assert_eq!(room(db.clone(), "p").await, Some((id.clone(), 1, 1)));
    td.delete("/api/hosts/helios").await.ok();
    let left: i64 = db
        .read(|c| Ok(c.query_row("SELECT count(*) FROM job_host_aliases", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(left, 0);
}

struct Lab {
    td: TestDaemon,
    _dirs: tempfile::TempDir,
    /// The bare repo: the remote.
    origin: PathBuf,
    /// A clone to make branches in.
    work: PathBuf,
    /// The project admin `p/lead`.
    lead: String,
    /// The worker `p/w1`.
    w1: String,
    /// The options of the host agent `lab1`, and its task.
    client_opts: ClientOptions,
    client: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The host listener of the daemon.
    hosts_addr: std::net::SocketAddr,
    /// The board flow of project `p`: `code` or `rules` (TER-170).
    flow: &'static str,
}

/// Drive queue picks directly in tests that exercise retries, not the timer.
struct QueueTicks(tokio::task::JoinHandle<()>);

impl QueueTicks {
    fn start(lab: &Lab) -> Self {
        let state = lab.td.daemon.as_ref().unwrap().state.clone();
        Self(tokio::spawn(async move {
            loop {
                if let Err(error) = terrariumd::jobs::tick(&state).await {
                    tracing::warn!(%error, "test queue tick failed");
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }))
    }
}

impl Drop for QueueTicks {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A daemon with host agent `lab1`, the project target `p-box`, project
/// `p` with repo `app` (a bare repo with `main` and `next`), the lead
/// and worker `w1`, and the queue set: `test -f OK`, `slots`.
async fn lab(slots: u32) -> Lab {
    lab_with(slots, None).await
}

/// [`lab`] with the board flow `flow` (`code` or `rules`) for project `p`.
async fn lab_flow(slots: u32, flow: &'static str) -> Lab {
    lab_impl(slots, None, flow).await
}

/// [`lab`] with a `docker` binary for the host agent.
async fn lab_with(slots: u32, docker: Option<&Path>) -> Lab {
    lab_impl(slots, docker, "code").await
}

async fn lab_impl(slots: u32, docker: Option<&Path>, flow: &'static str) -> Lab {
    lab_impl_ssh(slots, docker, flow, None).await
}

async fn lab_impl_ssh(
    slots: u32,
    docker: Option<&Path>,
    flow: &'static str,
    ssh: Option<&str>,
) -> Lab {
    let mut td = TestDaemon::start().await;
    if ssh.is_some() {
        td.boot_with_ssh(None, None, ssh).await;
    }
    let state = td.daemon.as_ref().unwrap().state.clone();
    let (_task, addr) = terrariumd::hosts::serve(state, "127.0.0.1:0")
        .await
        .unwrap();
    let r = td.post("/api/hosts", json!({"name": "lab1"})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let token = r.body["token"].as_str().unwrap().to_owned();
    let dirs = tempfile::tempdir().unwrap();
    let (o, client) =
        connect_host(&td, addr, "lab1", &token, &dirs.path().join("lab1"), docker).await;
    td.project("p").await;
    td.project("q").await;
    // The columns of the delivery process, as on the terrarium board.
    let r = td
        .patch(
            "/api/projects/p/board",
            json!({"columns": [
                {"name": "backlog"}, {"name": "ready"},
                {"name": "in_progress", "role": "working"}, {"name": "validating"},
                {"name": "review"}, {"name": "accepted"},
                {"name": "blocked", "role": "blocked"}, {"name": "done"}]}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let r = td
        .post(
            "/api/targets",
            json!({"name": "p-box", "kind": "host", "host": "lab1", "project": "p"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let r = td
        .post(
            "/api/targets",
            json!({"name": "q-box", "kind": "host", "host": "lab1", "project": "q"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let r = td
        .post(
            "/api/targets",
            json!({"name": "gbox", "kind": "host", "host": "lab1"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);

    let origin = dirs.path().join("origin.git");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "--bare", "-b", "main"]);
    let work = dirs.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("README"), "one\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "one"]);
    git(
        &work,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&work, &["push", "-q", "origin", "main", "main:next"]);
    let r = td
        .post(
            "/api/projects/p/repos",
            json!({"url": origin.display().to_string(), "name": "app"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);

    let lead = td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let w1 = td.agent("p", "w1", json!({})).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let work_dir = dirs.path().join("jobs");
    let r = td
        .put(
            "/api/projects/p/delivery",
            json!({"target": "p-box", "recipe": "test -f OK", "slots": slots,
                   "next_repos": ["app"], "work_dir": work_dir.display().to_string()}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    if flow != "code" {
        let r = td
            .patch("/api/projects/p", json!({"board_flow": flow}))
            .await;
        assert_eq!(r.status, 200, "{}", r.body);
        assert_eq!(r.body["board_flow"], flow, "{}", r.body);
        // The built-in pack has no validate rules (no validating column,
        // operator 2026-11-07); this board has the column, so the project
        // adds the opt-in ones, as a project that still validates does.
        let r = td
            .put(
                "/api/projects/p/rules",
                json!({"text": terrarium_core::rules_file::VALIDATE_RULES}),
            )
            .await;
        assert_eq!(r.status, 200, "{}", r.body);
    }
    Lab {
        td,
        _dirs: dirs,
        origin,
        work,
        lead,
        w1,
        client_opts: o,
        client: std::sync::Mutex::new(Some(client)),
        hosts_addr: addr,
        flow,
    }
}

/// Connect a host agent `name` with `token` to the daemon's host listener
/// `addr`, and wait until it is online. `dir` holds its instances and repos.
/// Returns its options and its task, to stop and start it again.
async fn connect_host(
    td: &TestDaemon,
    addr: std::net::SocketAddr,
    name: &str,
    token: &str,
    dir: &Path,
    docker: Option<&Path>,
) -> (ClientOptions, tokio::task::JoinHandle<()>) {
    let mut o = ClientOptions::new(format!("ws://{addr}/hosts/connect"), token, name);
    o.capabilities = HostCapabilities {
        shell: Some("/bin/sh".into()),
        ..Default::default()
    };
    if let Some(d) = docker {
        o.capabilities.docker = terrarium_core::protocol::ToolCapability {
            present: true,
            version: Some("29.8.0".into()),
        };
        o.docker_bin = d.display().to_string();
    }
    o.backoff_min = Duration::from_millis(50);
    o.backoff_max = Duration::from_millis(200);
    o.paths = terrarium_host::HostPaths {
        work_dir: td.state.with_file_name("work"),
        instances_dir: dir.join("instances"),
        repos_dir: dir.join("repos"),
    };
    let client = tokio::spawn(Client::new(o.clone()).run());
    let start = Instant::now();
    while td.get(&format!("/api/hosts/{name}")).await.body["online"] != true {
        assert!(start.elapsed() < DEADLINE, "{name} never came online");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    wait_admission(td, name).await;
    (o, client)
}

/// The link is online before reservation reconciliation opens admission
/// (TER-383): a job start that falls in that window is put back in the
/// queue, which under load moves the start by whole seconds. Wait, as
/// the host tests do, until the gate is open.
async fn wait_admission(td: &TestDaemon, name: &str) {
    let state = td.daemon.as_ref().unwrap().state.clone();
    let start = Instant::now();
    while state.executor.resource_pending(name) {
        assert!(start.elapsed() < DEADLINE, "{name} admission never opened");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

impl Lab {
    /// Wait until `/api/hosts/lab1` shows `online` as `on`.
    async fn until_online(&self, on: bool) {
        let start = Instant::now();
        while self.td.get("/api/hosts/lab1").await.body["online"] != on {
            assert!(start.elapsed() < DEADLINE, "lab1 online never became {on}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Stop the host agent `lab1`, as a daemon restart leaves it: not
    /// connected to the daemon.
    async fn agent_down(&self) {
        if let Some(c) = self.client.lock().unwrap().take() {
            c.abort();
        }
        self.until_online(false).await;
    }

    /// Start the host agent `lab1` again.
    async fn agent_up(&self) {
        let c = tokio::spawn(Client::new(self.client_opts.clone()).run());
        *self.client.lock().unwrap() = Some(c);
        self.until_online(true).await;
        wait_admission(&self.td, "lab1").await;
    }

    /// Make branch `name` from `base` with `files`, push it, and return
    /// its head.
    fn branch(&self, name: &str, base: &str, files: &[(&str, &str)]) -> String {
        git(&self.work, &["fetch", "-q", "origin"]);
        git(
            &self.work,
            &["checkout", "-q", "-B", name, &format!("origin/{base}")],
        );
        for (f, text) in files {
            std::fs::write(self.work.join(f), text).unwrap();
        }
        git(&self.work, &["add", "-A"]);
        git(&self.work, &["commit", "-q", "-m", name]);
        git(&self.work, &["push", "-q", "-f", "origin", name]);
        git(&self.work, &["rev-parse", "HEAD"])
    }

    /// A card in `backlog`, assigned to `w1`, on `branch`.
    async fn card(&self, title: &str, branch: &str) -> String {
        let r = self
            .td
            .post(
                "/api/projects/p/cards",
                json!({"title": title, "body": "the card body", "branch": branch,
                       "column": "backlog", "assignee": self.w1}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
        r.body["id"].as_str().unwrap().to_owned()
    }

    async fn card_of(&self, id: &str) -> Value {
        self.td.get(&format!("/api/cards/{id}")).await.ok().clone()
    }

    /// Poll the card until `f` holds. With `board_flow = rules` the rule
    /// engine then settles, so the later actions of the run that made `f`
    /// true (an unassign after a move) are there too.
    async fn until_card(&self, id: &str, what: &str, f: impl Fn(&Value) -> bool) -> Value {
        let start = Instant::now();
        loop {
            let c = self.card_of(id).await;
            if f(&c) {
                if self.flow == "code" {
                    return c;
                }
                self.settle().await;
                return self.card_of(id).await;
            }
            if start.elapsed() >= DEADLINE {
                let runs = self.td.get("/api/projects/p/rules/runs?limit=200").await;
                let runs: Vec<Value> = runs.body["runs"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|r| r["state"] != "noop" && r["card"] == id)
                    .collect();
                panic!("card {id} never got {what}; last {c}\nrule runs: {runs:#?}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn jobs(&self) -> Vec<Value> {
        self.td.get("/api/projects/p/jobs").await.ok()["jobs"]
            .as_array()
            .unwrap()
            .clone()
    }

    /// Wait until the rule engine read the whole log and ran each planned
    /// run (TER-170). With `board_flow = code` it has nothing to run.
    async fn settle(&self) {
        let db = self.td.daemon.as_ref().unwrap().state.db.clone();
        let start = Instant::now();
        let mut quiet = 0;
        loop {
            let (cursor, last, pending) = db
                .read(|c| {
                    let cursor = terrariumd::rules_engine::cursor(c)?.unwrap_or(0);
                    let last = terrariumd::events::last_seq(c)?;
                    let pending: i64 = c.query_row(
                        "SELECT count(*) FROM rule_runs WHERE state = 'pending'",
                        [],
                        |r| r.get(0),
                    )?;
                    Ok((cursor, last, pending))
                })
                .await
                .unwrap();
            if cursor >= last && pending == 0 {
                quiet += 1;
                if quiet >= 3 {
                    return;
                }
            } else {
                quiet = 0;
            }
            assert!(
                start.elapsed() < DEADLINE,
                "the rule engine did not settle: {cursor} {last} {pending}"
            );
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }

    /// The rule runs of project `p` that failed: none in a good flow.
    async fn failed_rule_runs(&self) -> Vec<Value> {
        let r = self.td.get("/api/projects/p/rules/runs?limit=500").await;
        if r.status == 404 {
            return Vec::new();
        }
        r.ok()["runs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["state"] == "failed")
            .cloned()
            .collect()
    }

    async fn until_idle(&self) -> Vec<Value> {
        let start = Instant::now();
        loop {
            let v = self.td.get("/api/projects/p/jobs").await.ok().clone();
            if v["running"] == 0 && v["queued"] == 0 {
                return v["jobs"].as_array().unwrap().clone();
            }
            assert!(start.elapsed() < DEADLINE, "the queue never drained: {v}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn labels(c: &Value) -> Vec<String> {
    c["labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap().to_owned())
        .collect()
}

fn comments(c: &Value) -> String {
    c["comments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["body"].as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn the_config_is_checked_and_audited() {
    let lab = lab(2).await;
    let td = &lab.td;
    let v = td.get("/api/projects/p/delivery").await;
    assert_eq!(v.ok()["delivery"]["recipe"], "test -f OK");
    assert_eq!(v.ok()["enabled"], true);
    assert_eq!(v.ok()["slots"], 2);
    // Another project's target: no.
    td.put(
        "/api/projects/p/delivery",
        json!({"target": "q-box", "recipe": "true"}),
    )
    .await
    .err(403, "forbidden");
    // A global target: the operator yes, a project admin no.
    td.put_as(
        &lab.lead,
        "/api/projects/p/delivery",
        json!({"target": "gbox", "recipe": "true"}),
    )
    .await
    .err(403, "forbidden");
    // A worker may not change it.
    td.put_as(
        &lab.w1,
        "/api/projects/p/delivery",
        json!({"target": "p-box", "recipe": "true"}),
    )
    .await
    .err(403, "forbidden");
    // Bad values.
    for pattern in ["", " \t"] {
        td.put(
            "/api/projects/p/delivery",
            json!({"target": "p-box", "failure_patterns": [pattern]}),
        )
        .await
        .err(422, "invalid");
    }
    td.put(
        "/api/projects/p/delivery",
        json!({"target": "p-box", "slots": 0}),
    )
    .await
    .err(422, "invalid");
    td.put(
        "/api/projects/p/delivery",
        json!({"target": "p-box", "next_repos": ["nope"]}),
    )
    .await
    .err(422, "invalid");
    // The lead changes the recipe: the audit keeps the old and the new.
    let r = td
        .put_as(
            &lab.lead,
            "/api/projects/p/delivery",
            json!({"target": "p-box", "recipe": "true", "next_repos": ["app"]}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let audit = td
        .get("/api/audit?action=project.delivery&order=desc")
        .await;
    let last = &audit.ok()[0]["detail"];
    assert_eq!(last["old"]["recipe"], "test -f OK", "{last}");
    assert_eq!(last["new"]["recipe"], "true", "{last}");
    // No recipe: the queue is off and a validate is refused.
    td.put("/api/projects/p/delivery", json!({"target": "p-box"}))
        .await
        .ok();
    let id = lab.card("c", "ter-x").await;
    let r = td
        .post_as(&lab.w1, &format!("/api/cards/{id}/validate"), json!({}))
        .await;
    r.err(409, "conflict");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("delivery set"),
        "{}",
        r.body
    );
}

both_flows!(a_green_validation_moves_the_card_to_review);
async fn a_green_validation_moves_the_card_to_review(flow: &'static str) {
    let lab = lab_flow(2, flow).await;
    let head = lab.branch("ter-a", "next", &[("OK", "yes\n")]);
    let id = lab.card("green", "ter-a").await;
    let gate = hold_jobs(&lab).await;
    // A commit that is not the head of the pushed branch is refused.
    let r = lab
        .td
        .post_as(
            &lab.w1,
            &format!("/api/cards/{id}/validate"),
            json!({"commit": "0123456789abcdef0123456789abcdef01234567"}),
        )
        .await;
    r.err(409, "conflict");
    // Another worker may not queue it.
    let w2 = lab.td.agent("p", "w2", json!({})).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    lab.td
        .post_as(
            &w2,
            &format!("/api/cards/{id}/validate"),
            json!({"commit": head}),
        )
        .await
        .err(403, "forbidden");
    let r = lab
        .td
        .post_as(
            &lab.w1,
            &format!("/api/cards/{id}/validate"),
            json!({"commit": &head[..12], "comment": "done by w1"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["job"]["kind"], "validate");
    assert_eq!(r.body["queue"]["position"], 1, "{}", r.body);
    assert_eq!(r.body["queue"]["slots"], 2);
    // Validating, still w1's, by:w1 and a job label.
    let c = lab
        .until_card(&id, "validating", |c| c["column"] == "validating")
        .await;
    assert_eq!(c["assignee"], lab.w1.as_str(), "{c}");
    let l = labels(&c);
    assert!(l.contains(&"by:w1".to_owned()), "{l:?}");
    assert!(l.iter().any(|l| l.starts_with("validation:")), "{l:?}");
    let own = c["comments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["body"] == "done by w1")
        .unwrap();
    assert_eq!(own["author"], lab.w1.as_str(), "{own:#}");
    std::fs::write(&gate, "").unwrap();
    let c = lab
        .until_card(&id, "review", |c| c["column"] == "review")
        .await;
    let l = labels(&c);
    assert!(!l.iter().any(|l| l.starts_with("validation:")), "{l:?}");
    assert!(
        c["assignee"].is_null(),
        "the final move frees the card: {c}"
    );
    let text = comments(&c);
    assert!(text.contains("passed"), "{text}");
    assert!(text.contains("done by w1"), "{text}");
    let jobs = lab.until_idle().await;
    let j = jobs.iter().find(|j| j["state"] == "passed").unwrap();
    assert_eq!(j["head"].as_str().unwrap(), head);
    assert_eq!(j["exit_code"], 0);
    lab.settle().await;
    assert_eq!(lab.failed_rule_runs().await, Vec::<Value>::new());
}

/// TER-155 D2 through the API: `eta` on a validate is a planned duration,
/// not a time. The card gets its ETA when the job starts, and the move to
/// `review` clears it again (D1).
#[tokio::test]
async fn a_validate_eta_is_a_duration_from_the_job_start() {
    let lab = lab(2).await;
    let head = lab.branch("ter-e", "next", &[("OK", "yes\n")]);
    let id = lab.card("eta", "ter-e").await;
    let path = format!("/api/cards/{id}/validate");
    for bad in ["2030-01-01T00:00:00Z", "soon", "31d"] {
        lab.td
            .post_as(&lab.w1, &path, json!({"commit": head, "eta": bad}))
            .await
            .err(422, "invalid");
    }
    assert!(lab.jobs().await.is_empty());
    let r = lab
        .td
        .post_as(&lab.w1, &path, json!({"commit": head, "eta": "20m"}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["job"]["planned_secs"], 1200, "{}", r.body);
    let c = lab
        .until_card(&id, "review", |c| c["column"] == "review")
        .await;
    assert!(c.get("eta_at").is_none(), "the move clears it: {c:#}");
    let jobs = lab.until_idle().await;
    let j = jobs.iter().find(|j| j["state"] == "passed").unwrap();
    let ev = lab
        .td
        .get(&format!("/api/cards/{id}/events"))
        .await
        .ok()
        .clone();
    let ev = ev.as_array().unwrap();
    let set = ev
        .iter()
        .find(|e| e["data"]["change"] == "eta" && e["data"]["job"] == true)
        .unwrap_or_else(|| panic!("no job ETA: {ev:#?}"));
    assert_eq!(
        secs_between(&j["started_at"], &set["data"]["eta_at"]),
        1200,
        "{set:#}"
    );
    let to_review = ev
        .iter()
        .find(|e| e["data"]["change"] == "moved" && e["data"]["to"] == "review")
        .unwrap();
    assert_eq!(
        to_review["data"]["previous_eta_at"], set["data"]["eta_at"],
        "{to_review:#}"
    );
}

both_flows!(the_job_queue_finds_its_columns_by_role_then_by_name);
/// TER-165: the daemon finds the columns by role. `validating` is renamed
/// and keeps its role. `review` has no role and `qa` has it, so the role
/// wins over the name. `accepted` has no role, so its name is the fallback.
async fn the_job_queue_finds_its_columns_by_role_then_by_name(flow: &'static str) {
    let lab = lab_flow(2, flow).await;
    let r = lab
        .td
        .patch(
            "/api/projects/p/board",
            json!({"columns": [
                {"name": "backlog"}, {"name": "ready"},
                {"name": "in_progress", "role": "working"},
                {"name": "checking", "rename_from": "validating", "role": "validating"},
                // A column that omits `role` keeps its old role: the default
                // board gives `review` the role `review`, so say null.
                {"name": "review", "role": null}, {"name": "qa", "role": "review"},
                {"name": "accepted", "role": null},
                {"name": "blocked", "role": "blocked"}, {"name": "done"}]}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    // A validate pass: the card is in the role column, then in `qa`.
    let head = lab.branch("ter-a", "next", &[("OK", "yes\n")]);
    let a = lab.card("by role", "ter-a").await;
    let gate = hold_jobs(&lab).await;
    let r = lab
        .td
        .post_as(
            &lab.w1,
            &format!("/api/cards/{a}/validate"),
            json!({"commit": head}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    lab.until_card(&a, "checking", |c| c["column"] == "checking")
        .await;
    std::fs::write(&gate, "").unwrap();
    lab.until_card(&a, "qa", |c| c["column"] == "qa").await;
    // A merge pass: the name `accepted` is the fallback.
    let head = lab.branch("ter-c", "next", &[("OK", "yes\n")]);
    let b = lab.card("by name", "ter-c").await;
    let r = lab
        .td
        .post_as(&lab.lead, &format!("/api/cards/{b}/merge"), json!({}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["job"]["kind"], "merge");
    lab.until_card(&b, "accepted", |c| c["column"] == "accepted")
        .await;
    let next = git(&lab.origin, &["rev-parse", "next"]);
    assert_ne!(next, head);
    lab.settle().await;
    assert_eq!(lab.failed_rule_runs().await, Vec::<Value>::new());
}

both_flows!(validate_replaces_stale_validation_and_by_labels);
/// TER-107: a validate leaves one `validation:*` label and one `by:` label,
/// even when the card already has stale ones.
async fn validate_replaces_stale_validation_and_by_labels(flow: &'static str) {
    let lab = lab_flow(2, flow).await;
    let head = lab.branch("ter-labels", "next", &[("OK", "yes\n")]);
    let id = lab.card("stale labels", "ter-labels").await;
    // Stale labels: another worker's `by:` and two `validation:` labels.
    lab.td
        .patch(
            &format!("/api/cards/{id}"),
            json!({"add_labels": ["by:w9", "validation:running", "validation:queued"]}),
        )
        .await
        .ok();
    // The seed is on the card before the validate, or the test proves nothing.
    let seeded = lab.card_of(&id).await;
    let l = labels(&seeded);
    assert!(l.contains(&"by:w9".to_owned()), "{seeded}");
    assert!(l.contains(&"validation:running".to_owned()), "{seeded}");
    let gate = hold_jobs(&lab).await;
    let r = lab
        .td
        .post_as(
            &lab.w1,
            &format!("/api/cards/{id}/validate"),
            json!({"commit": head}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let by_of = |c: &Value| -> Vec<String> {
        labels(c)
            .into_iter()
            .filter(|l| l.starts_with("by:"))
            .collect()
    };
    let validation_of = |c: &Value| -> Vec<String> {
        labels(c)
            .into_iter()
            .filter(|l| l.starts_with("validation:"))
            .collect()
    };
    // Still w1's, one by:w1, and one validation label (queued or already
    // running, the job may start at once).
    let c = lab
        .until_card(&id, "validating", |c| c["column"] == "validating")
        .await;
    assert_eq!(c["assignee"], lab.w1.as_str(), "{c}");
    assert_eq!(by_of(&c), ["by:w1"], "{c}");
    assert_eq!(validation_of(&c).len(), 1, "{c}");
    std::fs::write(&gate, "").unwrap();
    // The pass: no validation label, the assignee cleared, one by:w1.
    let c = lab
        .until_card(&id, "review", |c| c["column"] == "review")
        .await;
    assert!(c["assignee"].is_null(), "{c}");
    assert!(validation_of(&c).is_empty(), "{c}");
    assert_eq!(by_of(&c), ["by:w1"], "{c}");
}

both_flows!(an_admin_validate_keeps_the_workers_by_label);
/// TER-107: a project admin who queues another agent's card keeps the
/// card's worker in `by:`. The job and the final mail go to that worker.
async fn an_admin_validate_keeps_the_workers_by_label(flow: &'static str) {
    let lab = lab_flow(2, flow).await;
    let head = lab.branch("ter-admin", "next", &[("OK", "yes\n")]);
    let id = lab.card("admin queues", "ter-admin").await;
    // The worker's `by:` and a stale validation label, then the admin queues.
    lab.td
        .patch(
            &format!("/api/cards/{id}"),
            json!({"add_labels": ["by:w1", "validation:running"]}),
        )
        .await
        .ok();
    let seeded = lab.card_of(&id).await;
    assert!(labels(&seeded).contains(&"by:w1".to_owned()), "{seeded}");
    let gate = hold_jobs(&lab).await;
    let r = lab
        .td
        .post_as(
            &lab.lead,
            &format!("/api/cards/{id}/validate"),
            json!({"commit": head}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["job"]["by_agent"], "w1", "{}", r.body);
    let by_of = |c: &Value| -> Vec<String> {
        labels(c)
            .into_iter()
            .filter(|l| l.starts_with("by:"))
            .collect()
    };
    let c = lab
        .until_card(&id, "validating", |c| c["column"] == "validating")
        .await;
    assert_eq!(by_of(&c), ["by:w1"], "{c}");
    let validation = labels(&c)
        .into_iter()
        .filter(|l| l.starts_with("validation:"))
        .count();
    assert_eq!(validation, 1, "{c}");
    std::fs::write(&gate, "").unwrap();
    let c = lab
        .until_card(&id, "review", |c| c["column"] == "review")
        .await;
    assert_eq!(by_of(&c), ["by:w1"], "{c}");
    assert!(
        !labels(&c).iter().any(|l| l.contains("lead")),
        "the admin is not a worker label: {c}"
    );
}

both_flows!(a_red_validation_runs_twice_then_goes_to_the_top_of_ready);
async fn a_red_validation_runs_twice_then_goes_to_the_top_of_ready(flow: &'static str) {
    let lab = lab_flow(2, flow).await;
    // No OK file: the recipe fails.
    let head = lab.branch("ter-b", "next", &[("other", "x\n")]);
    let first = lab
        .td
        .post(
            "/api/projects/p/cards",
            json!({"title": "first in ready", "column": "ready"}),
        )
        .await
        .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let id = lab.card("red", "ter-b").await;
    let r = lab
        .td
        .post_as(
            &lab.w1,
            &format!("/api/cards/{id}/validate"),
            json!({"commit": head}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let c = lab
        .until_card(&id, "ready", |c| c["column"] == "ready")
        .await;
    let body = c["body"].as_str().unwrap();
    assert!(body.starts_with("Validation failed: "), "{body}");
    assert!(body.contains(&head), "{body}");
    assert!(body.contains("outcome red"), "{body}");
    assert!(body.contains("attempt 2"), "{body}");
    assert!(body.ends_with("the card body"), "{body}");
    assert!(c["assignee"].is_null(), "{c}");
    // At the top of ready.
    let b = lab.td.get("/api/projects/p/board").await;
    let ready: Vec<&str> = b.ok()["cards"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["column"] == "ready")
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    let first_pos = |x: &str| ready.iter().position(|c| *c == x).unwrap();
    assert!(first_pos(&id) < first_pos(&first), "{ready:?}");
    // Two tries, the second at the front of the queue.
    let jobs = lab.until_idle().await;
    let tries: Vec<i64> = jobs
        .iter()
        .filter(|j| j["card"] == id.as_str())
        .map(|j| j["attempt"].as_i64().unwrap())
        .collect();
    assert_eq!(tries.len(), 2, "{jobs:?}");
    assert!(jobs.iter().all(|j| j["state"] == "failed"), "{jobs:?}");
    // The worker gets a mail.
    let start = Instant::now();
    loop {
        let mail = lab.td.mail_of(&lab.w1).await;
        if mail.iter().any(|m| {
            m["subject"]
                .as_str()
                .unwrap_or("")
                .contains("validation failed")
        }) {
            break;
        }
        assert!(start.elapsed() < DEADLINE, "no mail to w1: {mail:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let c = lab.card_of(&id).await;
    let text = comments(&c);
    assert!(text.contains("It runs once more as job"), "{text}");
    assert!(text.contains("(attempt 2) failed again"), "{text}");
    assert!(
        !labels(&c).iter().any(|l| l.starts_with("validation:")),
        "{c}"
    );
    lab.settle().await;
    assert_eq!(lab.failed_rule_runs().await, Vec::<Value>::new());
}

/// TER-135: the stats give the first-attempt pass rate, the reruns, and
/// the failed test targets from the output tail.
#[tokio::test]
async fn the_stats_give_the_first_attempt_pass_rate_and_the_failed_targets() {
    let lab = lab(2).await;
    set_recipe(
        &lab,
        "test -f OK || { printf 'error: 1 target failed:\\n    `-p app --test flaky`\\n'; exit 101; }",
    )
    .await;
    let green = lab.branch("ter-g", "next", &[("OK", "yes\n")]);
    let red = lab.branch("ter-r", "next", &[("other", "x\n")]);
    for (title, branch, head) in [("green", "ter-g", &green), ("red", "ter-r", &red)] {
        let id = lab.card(title, branch).await;
        let r = lab
            .td
            .post_as(
                &lab.w1,
                &format!("/api/cards/{id}/validate"),
                json!({"commit": head}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
    }
    // The green job, and the red job with its one rerun.
    let start = Instant::now();
    while lab.until_idle().await.len() < 3 {
        assert!(start.elapsed() < DEADLINE, "no rerun");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    lab.until_idle().await;
    let r = lab
        .td
        .get_as(&lab.w1, "/api/projects/p/jobs/stats?since=1h")
        .await;
    let s = r.ok();
    assert_eq!(s["jobs"], 3, "{s}");
    assert_eq!(s["first_attempt"]["ended"], 2, "{s}");
    assert_eq!(s["first_attempt"]["passed"], 1, "{s}");
    assert_eq!(s["first_attempt"]["pass_rate"], 0.5, "{s}");
    assert_eq!(s["validate"]["ended"], 2, "{s}");
    assert_eq!(s["reruns"]["ended"], 1, "{s}");
    assert_eq!(s["reruns"]["passed"], 0, "{s}");
    assert_eq!(s["errors"], 0, "{s}");
    assert_eq!(s["untargeted_test_fails"], 0, "{s}");
    assert_eq!(
        s["failed_targets"],
        json!([{"target": "-p app --test flaky", "failed": 2, "first_attempt": 1}]),
        "{s}"
    );
    // The default window is 7 days. A time in the future has no jobs.
    assert_eq!(
        lab.td.get("/api/projects/p/jobs/stats").await.ok()["jobs"],
        3
    );
    let r = lab
        .td
        .get("/api/projects/p/jobs/stats?since=2999-01-01T00:00:00Z")
        .await;
    assert_eq!(r.ok()["jobs"], 0);
    assert!(r.ok()["first_attempt"]["pass_rate"].is_null());
    lab.td
        .get("/api/projects/p/jobs/stats?since=soon")
        .await
        .err(400, "bad_request");
}

both_flows!(a_merge_validates_and_pushes_next);
async fn a_merge_validates_and_pushes_next(flow: &'static str) {
    let lab = lab_flow(2, flow).await;
    let head = lab.branch("ter-c", "next", &[("OK", "yes\n")]);
    let id = lab.card("merge me", "ter-c").await;
    // Only an admin queues a merge.
    lab.td
        .post_as(&lab.w1, &format!("/api/cards/{id}/merge"), json!({}))
        .await
        .err(403, "forbidden");
    let r = lab
        .td
        .post_as(&lab.lead, &format!("/api/cards/{id}/merge"), json!({}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["job"]["kind"], "merge");
    lab.until_card(&id, "accepted", |c| c["column"] == "accepted")
        .await;
    // `next` on the remote holds the card commit, in a merge commit.
    let next = git(&lab.origin, &["rev-parse", "next"]);
    assert_ne!(next, head);
    git(&lab.origin, &["merge-base", "--is-ancestor", &head, "next"]);
    let parents = git(&lab.origin, &["rev-list", "--parents", "-n", "1", "next"]);
    assert_eq!(parents.split_whitespace().count(), 3, "{parents}");
    let jobs = lab.until_idle().await;
    assert_eq!(jobs[0]["head"].as_str().unwrap(), next);
    let c = lab
        .until_card(&id, "no job label", |c| {
            !labels(c).iter().any(|l| l.starts_with("validation:"))
        })
        .await;
    assert!(c["assignee"].is_null(), "{c}");
    let text = comments(&c);
    assert!(text.contains("Job queue: merge job"), "{text}");
    assert!(text.contains("passed"), "{text}");
    lab.settle().await;
    assert_eq!(lab.failed_rule_runs().await, Vec::<Value>::new());
}

both_flows!(a_conflict_is_not_resolved_and_not_rerun);
async fn a_conflict_is_not_resolved_and_not_rerun(flow: &'static str) {
    let lab = lab_flow(2, flow).await;
    // Two branches change the same line of README.
    lab.branch("ter-d1", "next", &[("README", "d1\n"), ("OK", "y\n")]);
    git(&lab.work, &["push", "-q", "origin", "ter-d1:next"]);
    let before = git(&lab.origin, &["rev-parse", "next"]);
    lab.branch("ter-d2", "main", &[("README", "d2\n"), ("OK", "y\n")]);
    let id = lab.card("conflict", "ter-d2").await;
    let r = lab
        .td
        .post(&format!("/api/cards/{id}/merge"), json!({}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let c = lab
        .until_card(&id, "ready", |c| c["column"] == "ready")
        .await;
    let body = c["body"].as_str().unwrap();
    assert!(body.starts_with("Validation failed: "), "{body}");
    assert!(body.contains("outcome conflict"), "{body}");
    assert!(body.contains("README"), "{body}");
    let jobs = lab.until_idle().await;
    assert_eq!(jobs.len(), 1, "a conflict is not rerun: {jobs:?}");
    assert_eq!(jobs[0]["outcome"], "conflict");
    // `next` did not move.
    assert_eq!(git(&lab.origin, &["rev-parse", "next"]), before);
    lab.settle().await;
    assert_eq!(lab.failed_rule_runs().await, Vec::<Value>::new());
}

#[tokio::test]
async fn jobs_keep_to_the_slots_in_order() {
    let lab = lab(1).await;
    // The recipe takes a moment, so the jobs would overlap without slots.
    lab.td
        .put(
            "/api/projects/p/delivery",
            json!({"target": "p-box", "recipe": "test -f OK && sleep 1", "slots": 1,
                   "next_repos": ["app"],
                   "work_dir": lab._dirs.path().join("jobs").display().to_string()}),
        )
        .await
        .ok();
    let mut cards = Vec::new();
    for n in ["e1", "e2", "e3"] {
        let head = lab.branch(&format!("ter-{n}"), "next", &[("OK", n)]);
        let id = lab.card(n, &format!("ter-{n}")).await;
        let r = lab
            .td
            .post_as(
                &lab.w1,
                &format!("/api/cards/{id}/validate"),
                json!({"commit": head}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
        cards.push(id);
    }
    // While they run, never more than one runs.
    let start = Instant::now();
    loop {
        let v = lab.td.get("/api/projects/p/jobs").await.ok().clone();
        assert!(v["running"].as_i64().unwrap() <= 1, "{v}");
        if v["running"] == 0 && v["queued"] == 0 {
            break;
        }
        assert!(start.elapsed() < DEADLINE, "the queue never drained: {v}");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    // First in, first out, with no overlap.
    let mut jobs = lab.jobs().await;
    jobs.sort_by(|a, b| {
        a["started_at"]
            .as_str()
            .unwrap()
            .cmp(b["started_at"].as_str().unwrap())
    });
    let order: Vec<&str> = jobs.iter().map(|j| j["card"].as_str().unwrap()).collect();
    assert_eq!(order, cards.iter().map(String::as_str).collect::<Vec<_>>());
    for w in jobs.windows(2) {
        assert!(
            w[1]["started_at"].as_str().unwrap() >= w[0]["ended_at"].as_str().unwrap(),
            "{w:?}"
        );
    }
    assert!(jobs.iter().all(|j| j["state"] == "passed"), "{jobs:?}");
    for id in &cards {
        assert_eq!(lab.card_of(id).await["column"], "review");
    }
}

both_flows!(a_cancel_ends_a_queued_job);
async fn a_cancel_ends_a_queued_job(flow: &'static str) {
    let lab = lab_flow(1, flow).await;
    lab.td
        .put(
            "/api/projects/p/delivery",
            json!({"target": "p-box", "recipe": "sleep 30", "slots": 1}),
        )
        .await
        .ok();
    let h1 = lab.branch("ter-f1", "next", &[("OK", "1")]);
    let h2 = lab.branch("ter-f2", "next", &[("OK", "2")]);
    let c1 = lab.card("f1", "ter-f1").await;
    let c2 = lab.card("f2", "ter-f2").await;
    let j1 = lab
        .td
        .post_as(
            &lab.w1,
            &format!("/api/cards/{c1}/validate"),
            json!({"commit": h1}),
        )
        .await
        .ok()["job"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // Job 1 must hold the only slot before job 2 is posted. Under load
    // its start can wait for its host or the host admission (TER-160,
    // TER-383), and job 2 would queue beside or ahead of it, not behind
    // it, and the position below would not be 1.
    until_running(&lab, &j1).await;
    let r = lab
        .td
        .post_as(
            &lab.w1,
            &format!("/api/cards/{c2}/validate"),
            json!({"commit": h2}),
        )
        .await;
    assert_eq!(r.body["queue"]["position"], 1, "{}", r.body);
    let j2 = r.body["job"]["id"].as_str().unwrap().to_owned();
    // A card has one open job at a time.
    lab.td
        .post(&format!("/api/cards/{c2}/validate"), json!({}))
        .await
        .err(409, "conflict");
    // The worker cancels its queued job; then the running one (killed).
    let r = lab.td.delete(&format!("/api/jobs/{j2}")).await;
    assert_eq!(r.ok()["state"], "cancelled");
    until_running(&lab, &j1).await;
    let r = lab.td.delete(&format!("/api/jobs/{j1}")).await;
    assert_eq!(r.ok()["state"], "cancelled");
    let jobs = lab.until_idle().await;
    assert!(jobs.iter().all(|j| j["state"] == "cancelled"), "{jobs:?}");
    // The cards stay where they are, without the job labels.
    lab.settle().await;
    for id in [&c1, &c2] {
        let c = lab.card_of(id).await;
        assert_eq!(c["column"], "validating");
        assert!(
            !labels(&c).iter().any(|l| l.starts_with("validation:")),
            "{c}"
        );
    }
    assert_eq!(lab.failed_rule_runs().await, Vec::<Value>::new());
}

/// TER-97: the host cap counts the jobs of all projects on one host. `p`
/// and `q` each have 2 slots, on targets of the host agent `lab1`, with a
/// cap of 1 on `lab1`: never more than one job runs, and the jobs start in
/// the order they were queued, across the projects. The recipe checks that
/// it runs 19 steps nicer than this process (at most 19), and in the idle
/// IO class when the machine has a working `ionice -c3`.
#[tokio::test]
async fn the_host_cap_holds_across_projects_in_order() {
    let lab = lab(2).await;
    let host_id = lab.td.get("/api/hosts/lab1").await.body["id"].clone();
    let wd = lab._dirs.path().join("jobs").display().to_string();
    // `nice -n19` adds 19 to the niceness of the host agent (this test
    // process), at most 19: a run under a parent at -4 gets 15.
    let base: i32 = String::from_utf8(std::process::Command::new("nice").output().unwrap().stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let want = (base + 19).min(19);
    let recipe = format!(
        "test -f OK && test \"$(nice)\" = {want} \
         && {{ ! ionice -c3 true 2>/dev/null || ionice -p $$ | grep -q idle; }} \
         && sleep 1"
    );
    let recipe = recipe.as_str();
    lab.td
        .put(
            "/api/projects/p/delivery",
            json!({"target": "p-box", "recipe": recipe, "slots": 2, "work_dir": wd}),
        )
        .await
        .ok();
    let r = lab
        .td
        .patch(
            "/api/projects/q/board",
            json!({"columns": [
                {"name": "backlog"}, {"name": "ready"},
                {"name": "in_progress", "role": "working"}, {"name": "validating"},
                {"name": "review"}, {"name": "accepted"},
                {"name": "blocked", "role": "blocked"}, {"name": "done"}]}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let r = lab
        .td
        .post(
            "/api/projects/q/repos",
            json!({"url": lab.origin.display().to_string(), "name": "app"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    lab.td
        .put(
            "/api/projects/q/delivery",
            json!({"target": "q-box", "recipe": recipe, "slots": 2, "work_dir": wd}),
        )
        .await
        .ok();

    // Only a sysadmin or the operator sets a cap; 0 is not a cap.
    lab.td
        .put_as(&lab.lead, "/api/job-hosts/lab1", json!({"heavy_jobs": 1}))
        .await
        .err(403, "forbidden");
    lab.td
        .put("/api/job-hosts/lab1", json!({"heavy_jobs": 0}))
        .await
        .err(422, "invalid");
    let r = lab
        .td
        .put("/api/job-hosts/lab1", json!({"heavy_jobs": 1}))
        .await;
    assert_eq!(r.ok()["hosts"][0]["host"], host_id, "{}", r.body);
    assert_eq!(r.ok()["hosts"][0]["heavy_jobs"], 1, "{}", r.body);
    let audit = lab
        .td
        .get("/api/audit?action=job.host_cap&order=desc")
        .await;
    assert_eq!(audit.ok()[0]["detail"]["new"], 1, "{}", audit.body);

    // Queue p, q, p, q: the operator queues (a project admin of both).
    let mut cards = Vec::new();
    for (n, project) in [("h1", "p"), ("h2", "q"), ("h3", "p"), ("h4", "q")] {
        let branch = format!("ter-{n}");
        let head = lab.branch(&branch, "next", &[("OK", n)]);
        let r = lab
            .td
            .post(
                &format!("/api/projects/{project}/cards"),
                json!({"title": n, "branch": branch, "column": "backlog"}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
        let id = r.body["id"].as_str().unwrap().to_owned();
        let r = lab
            .td
            .post(
                &format!("/api/cards/{id}/validate"),
                json!({"commit": head, "repo": "app"}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
        assert_eq!(r.body["queue"]["host"]["host"], host_id, "{}", r.body);
        assert_eq!(r.body["queue"]["host"]["heavy_jobs"], 1, "{}", r.body);
        cards.push(id);
    }

    // While they run, never more than one runs on lab1.
    let all = |v: &Value| v.as_i64().unwrap();
    let start = Instant::now();
    loop {
        let p = lab.td.get("/api/projects/p/jobs").await.ok().clone();
        let q = lab.td.get("/api/projects/q/jobs").await.ok().clone();
        let h = lab.td.get("/api/job-hosts").await.ok().clone();
        let running = all(&p["running"]) + all(&q["running"]);
        assert!(running <= 1, "p {p}\nq {q}");
        assert!(h["hosts"][0]["running"].as_i64().unwrap() <= 1, "{h}");
        if running == 0 && all(&p["queued"]) + all(&q["queued"]) == 0 {
            break;
        }
        assert!(
            start.elapsed() < DEADLINE,
            "the queues never drained: p {p}\nq {q}"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    // First in, first out across the projects, with no overlap.
    let mut jobs: Vec<Value> = Vec::new();
    for project in ["p", "q"] {
        let v = lab
            .td
            .get(&format!("/api/projects/{project}/jobs"))
            .await
            .ok()
            .clone();
        jobs.extend(v["jobs"].as_array().unwrap().iter().cloned());
    }
    assert!(
        jobs.iter()
            .all(|j| j["state"] == "passed" && j["host"] == host_id),
        "{jobs:#?}"
    );
    jobs.sort_by(|a, b| {
        a["started_at"]
            .as_str()
            .unwrap()
            .cmp(b["started_at"].as_str().unwrap())
    });
    let order: Vec<&str> = jobs.iter().map(|j| j["card"].as_str().unwrap()).collect();
    assert_eq!(order, cards.iter().map(String::as_str).collect::<Vec<_>>());
    for w in jobs.windows(2) {
        assert!(
            w[1]["started_at"].as_str().unwrap() >= w[0]["ended_at"].as_str().unwrap(),
            "{w:?}"
        );
    }
    // The wrapper is on the job.
    let id = jobs[0]["id"].as_str().unwrap();
    let j = lab.td.get(&format!("/api/jobs/{id}")).await.ok().clone();
    let tail = j["tail"].as_str().unwrap();
    assert!(tail.contains("nice -n19 sh -c <recipe>]"), "{tail}");

    // The cap goes away: the host has no cap and no running job.
    let r = lab.td.delete("/api/job-hosts/lab1").await;
    assert_eq!(r.ok()["hosts"], json!([]), "{}", r.body);
}

/// TER-190: each slot has one work tree, `<work_dir>/p/slot-<n>`, kept from
/// job to job. Four jobs on two slots: each recipe runs in the tree of
/// its `TERRARIUM_JOB_SLOT`, at its own commit, with no file left from the
/// job before but `target/`, which keeps what the jobs before wrote. No
/// two jobs hold one tree at once (`target/busy`), and no dir per job is
/// made. A merge runs in the tree of slot 0.
#[tokio::test]
async fn slots_keep_their_tree_and_never_share_it() {
    let lab = lab(2).await;
    let wd = lab._dirs.path().join("jobs");
    let recipe = r#"set -e
test "$(basename "$(pwd -P)")" = "slot-$TERRARIUM_JOB_SLOT"
test ! -e junk
test "$(git rev-parse HEAD)" = "$TERRARIUM_HEAD"
mkdir -p target
mkdir target/busy
echo "$TERRARIUM_JOB_SLOT $TERRARIUM_HEAD" >> target/log
echo x > junk
sleep 1
test "$(git rev-parse HEAD)" = "$TERRARIUM_HEAD"
rmdir target/busy
test -f OK"#;
    lab.td
        .put(
            "/api/projects/p/delivery",
            json!({"target": "p-box", "recipe": recipe, "slots": 2,
                   "next_repos": ["app"], "work_dir": wd.display().to_string()}),
        )
        .await
        .ok();
    let mut heads = Vec::new();
    for n in ["s1", "s2", "s3", "s4"] {
        let head = lab.branch(&format!("ter-{n}"), "next", &[("OK", n)]);
        let id = lab.card(n, &format!("ter-{n}")).await;
        let r = lab
            .td
            .post_as(
                &lab.w1,
                &format!("/api/cards/{id}/validate"),
                json!({"commit": head}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
        heads.push(head);
    }
    let jobs = lab.until_idle().await;
    assert!(jobs.iter().all(|j| j["state"] == "passed"), "{jobs:#?}");
    let mut slots: Vec<i64> = jobs.iter().map(|j| j["slot"].as_i64().unwrap()).collect();
    slots.sort_unstable();
    slots.dedup();
    assert_eq!(slots, [1, 2], "{jobs:#?}");
    // Each tree's target/ kept the lines of all its jobs, of its slot only.
    let mut seen = Vec::new();
    let mut most = 0;
    for s in [1, 2] {
        let log = std::fs::read_to_string(wd.join(format!("p/slot-{s}/target/log"))).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        most = most.max(lines.len());
        for l in lines {
            let (slot, head) = l.split_once(' ').unwrap();
            assert_eq!(slot, s.to_string(), "{log}");
            seen.push(head.to_owned());
        }
    }
    assert!(most >= 2, "no tree was used twice");
    seen.sort();
    heads.sort();
    assert_eq!(seen, heads);
    let mut dirs: Vec<String> = std::fs::read_dir(wd.join("p"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    dirs.sort();
    assert_eq!(dirs, ["logs", "slot-1", "slot-2"]);
    // Logs belong to jobs, not slots, and survive later slot checkouts.
    for job in &jobs {
        let id = job["id"].as_str().unwrap();
        let attempt = job["attempt"].as_i64().unwrap();
        assert!(wd.join(format!("p/logs/{id}-{attempt}.log")).is_file());
    }

    let job = merge_with_recipe(
        &lab,
        "ter-s5",
        "test \"$TERRARIUM_JOB_SLOT\" = 0 && test \"$(basename \"$(pwd -P)\")\" = slot-0 && test -f OK",
    )
    .await;
    assert_eq!(job["state"], "passed", "{job:#}");
    assert_eq!(job["slot"], 0, "{job:#}");
}

#[tokio::test]
async fn a_failed_job_keeps_the_full_log_and_puts_early_panics_on_the_card() {
    let lab = lab(1).await;
    let recipe = r#"printf 'test early_failure ... FAILED\n'
printf "thread 'early_failure' panicked at test.rs:1:2:\nexpected value\n" >&2
i=0; while [ "$i" -lt 2000 ]; do printf 'unrelated build noise\n'; i=$((i + 1)); done
exit 101"#;
    let job = merge_with_recipe(&lab, "ter-log-failure", recipe).await;
    assert_eq!(job["state"], "failed", "{job:#}");
    assert_eq!(job["exit_code"], 101, "{job:#}");
    let id = job["id"].as_str().unwrap();
    let attempt = job["attempt"].as_i64().unwrap();
    let log = lab
        ._dirs
        .path()
        .join(format!("jobs/p/logs/{id}-{attempt}.log"));
    let full = std::fs::read_to_string(&log).unwrap();
    assert_eq!(full.matches("unrelated build noise").count(), 2000);
    assert!(full.contains("expected value"));
    let shown = lab.td.get(&format!("/api/jobs/{id}")).await.ok().clone();
    let tail = shown["tail"].as_str().unwrap();
    assert!(tail.contains(&log.display().to_string()), "{tail}");
    assert!(tail.contains("Full log on target p-box:"), "{tail}");
    let card = lab
        .until_card(job["card"].as_str().unwrap(), "failure block", |c| {
            c["body"]
                .as_str()
                .is_some_and(|body| body.starts_with("Validation failed:"))
        })
        .await;
    let body = card["body"].as_str().unwrap();
    for expected in [
        "test early_failure ... FAILED",
        "panicked at test.rs:1:2",
        "expected value",
        "Full log on target p-box:",
    ] {
        assert!(body.contains(expected), "missing {expected}: {body}");
    }
}

/// Queue a merge of a new card on `branch` with `recipe`, wait for the
/// end, and return the job.
async fn merge_with_recipe(lab: &Lab, branch: &str, recipe: &str) -> Value {
    lab.td
        .put(
            "/api/projects/p/delivery",
            json!({"target": "p-box", "recipe": recipe, "next_repos": ["app"],
                   "work_dir": lab._dirs.path().join("jobs").display().to_string()}),
        )
        .await
        .ok();
    lab.branch(branch, "next", &[("OK", "yes\n")]);
    let id = lab.card(branch, branch).await;
    let r = lab
        .td
        .post_as(&lab.lead, &format!("/api/cards/{id}/merge"), json!({}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let jobs = lab.until_idle().await;
    jobs.into_iter().find(|j| j["card"] == id.as_str()).unwrap()
}

#[tokio::test]
#[ignore = "requires an isolated scratch SSH endpoint with real Docker"]
async fn live_docker_through_daemon_tunnel_and_release_relay() {
    // The scratch exercise supplies a temporary, local-only SSH identity and
    // sshd. Never use the production Docker CI or reset identities here.
    let ssh = std::env::var("TER363_SCRATCH_SSH").unwrap();
    let key = std::fs::read_to_string(std::env::var("TER363_SCRATCH_KEY").unwrap()).unwrap();
    let pin = std::fs::read_to_string(std::env::var("TER363_SCRATCH_PIN").unwrap()).unwrap();
    let port: u16 = std::env::var("TER363_SCRATCH_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let lab = lab_impl_ssh(1, None, "code", Some(&ssh)).await;
    for name in ["vm", "reset"] {
        lab.td
            .put(
                &format!("/api/secrets/p/{name}"),
                json!({"kind":"ssh_key", "fields":{"private_key":key}}),
            )
            .await
            .ok();
        lab.td
            .post(
                &format!("/api/secrets/p/{name}/grants"),
                json!({"grantee":"delivery:p", "level":"run", "targets":["p-box"]}),
            )
            .await
            .ok();
    }
    let recipe = r#"set -eu
docker version
docker pull alpine:3.21
name=ter-363-relay-$$
trap 'docker rm -f "$name" >/dev/null 2>&1 || true' EXIT
docker run --name "$name" alpine:3.21 sh -c 'yes ter-363-large-log | head -c 8388608'
test "$(docker logs "$name" | wc -c)" -eq 8388608
echo real-tunnel-large-log-ok
"#;
    let marker = lab._dirs.path().join("live-recipe-started");
    let mut config = json!({"target":"p-box", "recipe":format!("touch '{}'\n{recipe}", marker.display()), "next_repos":["app"], "work_dir":lab._dirs.path().join("jobs"), "merge_secrets":{"DOCKER_CI_KEY":"@secret:vm.private_key", "DOCKER_CI_RESET_KEY":"@secret:reset.private_key"}, "docker_ci":{"destination":"root@127.0.0.1", "port":port, "known_hosts":pin, "reset_destination":"root@127.0.0.1", "reset_port":port,"reset_known_hosts":pin}});
    let wrong_pin =
        std::fs::read_to_string(std::env::var("TER363_SCRATCH_WRONG_PIN").unwrap()).unwrap();
    for (branch, reset_pin) in [
        (
            "reset-unknown-host",
            pin.replace("127.0.0.1", "unknown-host"),
        ),
        ("reset-wrong-key", wrong_pin),
    ] {
        config["docker_ci"]["reset_known_hosts"] = json!(reset_pin);
        lab.td
            .put("/api/projects/p/delivery", config.clone())
            .await
            .ok();
        lab.branch(branch, "next", &[(branch, "yes\n")]);
        let card = lab.card(branch, branch).await;
        lab.td
            .post_as(&lab.lead, &format!("/api/cards/{card}/merge"), json!({}))
            .await
            .ok();
        let jobs = lab.until_idle().await;
        let job = jobs.iter().find(|j| j["card"] == card).unwrap();
        assert_ne!(job["state"], "passed", "{job:#}");
        assert!(
            !marker.exists(),
            "reset pin failure must prevent the recipe"
        );
        let detail = lab
            .td
            .get(&format!("/api/jobs/{}", job["id"].as_str().unwrap()))
            .await;
        assert!(detail.ok().to_string().contains("reset failed"));
    }
    config["docker_ci"]["reset_known_hosts"] = json!(pin);
    lab.td.put("/api/projects/p/delivery", config).await.ok();
    lab.branch("live-relay", "next", &[("live-relay", "yes\n")]);
    let card = lab.card("live relay", "live-relay").await;
    lab.td
        .post_as(&lab.lead, &format!("/api/cards/{card}/merge"), json!({}))
        .await
        .ok();
    let jobs = lab.until_idle().await;
    let job = jobs.iter().find(|j| j["card"] == card).unwrap();
    assert_eq!(job["state"], "passed", "{job:#}");
    let detail = lab
        .td
        .get(&format!("/api/jobs/{}", job["id"].as_str().unwrap()))
        .await;
    let detail = detail.ok().to_string();
    assert!(detail.contains("real-tunnel-large-log-ok"), "{detail}");
    assert!(!detail.contains(&key));
}

/// A daemon-tracing capture shared by this whole test binary (TER-425):
/// `set_global_default` succeeds only once per process, so a test that
/// installs its own subscriber makes every later install panic. The layer
/// is installed exactly once, through a `OnceLock`; acquiring the handle
/// empties the buffer, so each test captures its own run and drains it
/// with `take`. Tests that drain it must not run at the same time.
#[derive(Clone)]
struct TraceCapture(std::sync::Arc<std::sync::Mutex<Option<Vec<u8>>>>);
impl std::io::Write for TraceCapture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if let Some(buffer) = self.0.lock().unwrap().as_mut() {
            buffer.extend_from_slice(bytes);
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

static TRACE_CAPTURE: std::sync::OnceLock<TraceCapture> = std::sync::OnceLock::new();

/// Global to include job-queue and executor tasks spawned by the daemon.
fn daemon_trace_capture() -> TraceCapture {
    let capture = TRACE_CAPTURE
        .get_or_init(|| {
            let buffer = std::sync::Arc::new(std::sync::Mutex::new(Some(Vec::new())));
            let capture = TraceCapture(buffer);
            let writer = capture.clone();
            tracing::subscriber::set_global_default(
                tracing_subscriber::fmt()
                    .with_ansi(false)
                    .with_writer(move || writer.clone())
                    .finish(),
            )
            .expect("the shared capture is the only global subscriber");
            capture
        })
        .clone();
    *capture.0.lock().unwrap() = Some(Vec::new());
    capture
}

#[tokio::test]
async fn merge_job_resolves_daemon_credentials_resets_before_tunnel_and_revokes() {
    use std::os::unix::fs::PermissionsExt;
    let trace = daemon_trace_capture();
    let fixtures = tempfile::tempdir().unwrap();
    let ssh = fixtures.path().join("ssh");
    std::fs::write(
        &ssh,
        format!(
            r#"#!/usr/bin/env python3
import os, pathlib, socket, sys, time
root = pathlib.Path({root:?})
args = sys.argv[1:]
key = pathlib.Path(args[args.index('-i') + 1]).read_text()
assert 'SSH_AUTH_SOCK' not in os.environ
if '-L' not in args:
    assert key == 'dummy-reset-key-363'
    assert 'StrictHostKeyChecking=yes' in args
    pin_arg = next(a for a in args if a.startswith('UserKnownHostsFile='))
    pin_path = pathlib.Path(pin_arg.split('=', 1)[1])
    assert pin_path.parent == pathlib.Path(args[args.index('-i') + 1]).parent
    pin = pin_path.read_text()
    with (root / 'order').open('a') as f: f.write('reset\n')
    if not pin.startswith('test-host '): sys.exit(255)
    if 'public-test-pin' not in pin: sys.exit(255)
    status = root / 'status'
    code = int(status.read_text()) if status.exists() else 0
    if code == 75: status.write_text('0')
    if code == 175: code = 75
    sys.exit(code)
assert key == 'dummy-tunnel-key-363'
assert (root / 'order').read_text().endswith('reset\n')
with (root / 'order').open('a') as f: f.write('tunnel\n')
s = socket.socket(socket.AF_UNIX)
s.bind(args[args.index('-L') + 1].split(':')[0])
s.listen()
while True: time.sleep(1)
"#,
            root = fixtures.path().to_str().unwrap()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let lab = lab_impl_ssh(1, None, "code", Some(ssh.to_str().unwrap())).await;
    for (name, value) in [
        ("vm", "dummy-tunnel-key-363"),
        ("reset", "dummy-reset-key-363"),
    ] {
        lab.td
            .put(
                &format!("/api/secrets/p/{name}"),
                json!({"kind":"api_key", "fields":{"value":value}}),
            )
            .await
            .ok();
        lab.td
            .post(
                &format!("/api/secrets/p/{name}/grants"),
                json!({"grantee":"delivery:p", "level":"run", "targets":["p-box"]}),
            )
            .await
            .ok();
    }
    let marker = fixtures.path().join("recipe-started");
    let environment = fixtures.path().join("recipe-env.json");
    let relay_snapshot = fixtures.path().join("relay-files.json");
    let recipe = format!(
        r#"test -z "${{DOCKER_CI_KEY:-}}${{DOCKER_CI_RESET_KEY:-}}${{SSH_AUTH_SOCK:-}}" && test -S "${{DOCKER_HOST#unix://}}" && python3 -I -c 'import json,os,pathlib; pathlib.Path("{}").write_text(json.dumps(dict(os.environ))); directory=pathlib.Path(os.environ["DOCKER_HOST"].removeprefix("unix://")).parent; pathlib.Path("{}").write_text(json.dumps([str(p) for p in directory.iterdir()])); assert all(not p.is_file() for p in directory.iterdir())' && touch '{}' && echo recipe-ok"#,
        environment.display(),
        relay_snapshot.display(),
        marker.display()
    );
    let mut config = json!({"target":"p-box", "recipe":recipe, "next_repos":["app"], "work_dir":lab._dirs.path().join("jobs"), "merge_secrets":{"DOCKER_CI_KEY":"@secret:vm", "DOCKER_CI_RESET_KEY":"@secret:reset"}, "docker_ci":{"destination":"root@test-vm", "port":2250, "known_hosts":"test-vm ssh-ed25519 public-test-pin\n", "reset_destination":"reset@test-host", "reset_port":22,"reset_known_hosts":"test-host ssh-ed25519 public-test-pin\n"}});
    for (branch, status, expected) in [
        ("secret-ok", 0, true),
        ("secret-reset-failed", 1, false),
        ("secret-reset-busy", 75, true),
        ("secret-reset-exhausted", 175, false),
        ("secret-reset-unknown-pin", 255, false),
        ("secret-reset-wrong-pin", 256, false),
        ("secret-revoked", 0, false),
    ] {
        std::fs::write(fixtures.path().join("status"), status.to_string()).unwrap();
        let order_before = std::fs::read_to_string(fixtures.path().join("order"))
            .unwrap_or_default()
            .len();
        config["docker_ci"]["reset_known_hosts"] = match status {
            255 => json!("other-host ssh-ed25519 public-test-pin\n"),
            256 => json!("test-host ssh-ed25519 wrong-key\n"),
            _ => json!("test-host ssh-ed25519 public-test-pin\n"),
        };
        lab.td
            .put("/api/projects/p/delivery", config.clone())
            .await
            .ok();
        let _ = std::fs::remove_file(&marker);
        if branch == "secret-revoked" {
            lab.td
                .delete("/api/secrets/p/vm/grants?grantee=delivery:p")
                .await
                .ok();
        }
        lab.branch(branch, "next", &[(branch, "yes\n")]);
        let card = lab.card(branch, branch).await;
        lab.td
            .post_as(&lab.lead, &format!("/api/cards/{card}/merge"), json!({}))
            .await
            .ok();
        let jobs = lab.until_idle().await;
        let job = jobs.iter().find(|j| j["card"] == card).unwrap();
        assert_eq!(job["state"] == "passed", expected, "{job:#}");
        assert_eq!(marker.exists(), expected);
        let detail = lab
            .td
            .get(&format!("/api/jobs/{}", job["id"].as_str().unwrap()))
            .await;
        let text = detail.ok().to_string();
        for key in ["dummy-reset-key-363", "dummy-tunnel-key-363"] {
            assert!(!text.contains(key), "{text}");
        }
        if status == 1 {
            assert!(text.contains("reset failed"), "{text}");
        }
        if status == 175 {
            assert!(text.contains("lock busy"));
            let order = std::fs::read_to_string(fixtures.path().join("order")).unwrap();
            let attempts = &order[order_before..];
            assert!(!attempts.contains("tunnel"));
            // Other failed cards may be retried in this queue cycle, but each
            // job must finish after exactly three busy attempts.
            assert!(attempts.lines().count() >= 3);
            assert_eq!(attempts.lines().count() % 3, 0);
        }
        if expected {
            assert!(text.contains("Full log on target"), "{text}");
            let env: std::collections::BTreeMap<String, String> =
                serde_json::from_slice(&std::fs::read(&environment).unwrap()).unwrap();
            for value in env.values() {
                assert!(
                    !value.contains("dummy-reset-key-363")
                        && !value.contains("dummy-tunnel-key-363")
                );
            }
            let inherited: std::collections::BTreeMap<_, _> = std::env::vars().collect();
            let ordinary = [
                "GIT_AUTHOR_NAME",
                "GIT_AUTHOR_EMAIL",
                "GIT_COMMITTER_NAME",
                "GIT_COMMITTER_EMAIL",
                "TERRARIUM_JOB_KIND",
                "TERRARIUM_JOB_ID",
                "TERRARIUM_JOB_SLOT",
                "TERRARIUM_HEAD",
                "TERRARIUM_BASE",
                "TERRARIUM_CHANGED_FILES",
                "PWD",
                "OLDPWD",
                "SHLVL",
                "_",
            ];
            let added: Vec<_> = env
                .keys()
                .filter(|k| !inherited.contains_key(*k) && !ordinary.contains(&k.as_str()))
                .map(String::as_str)
                .collect();
            assert_eq!(added, ["DOCKER_HOST"]);
            assert!(
                std::fs::read_to_string(&relay_snapshot)
                    .unwrap()
                    .contains("docker.sock")
            );
        }
    }
    let audit = lab.td.get("/api/audit").await;
    let text = audit.ok().to_string();
    assert!(text.contains("delivery:p"), "{text}");
    for key in ["dummy-reset-key-363", "dummy-tunnel-key-363"] {
        assert!(!text.contains(key));
    }
    let events = lab
        .td
        .daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .read(|c| {
            let mut statement =
                c.prepare("SELECT summary || payload FROM events WHERE project = 'p'")?;
            let rows = statement.query_map([], |r| r.get::<_, String>(0))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?.join("\n"))
        })
        .await
        .unwrap();
    for key in ["dummy-reset-key-363", "dummy-tunnel-key-363"] {
        assert!(!events.contains(key), "{events}");
    }
    fn scan(path: &Path, logs: &mut usize) {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                scan(&entry.path(), logs);
            } else if kind.is_file() {
                let bytes = std::fs::read(entry.path()).unwrap();
                for key in [
                    b"dummy-reset-key-363".as_slice(),
                    b"dummy-tunnel-key-363".as_slice(),
                ] {
                    assert!(
                        !bytes.windows(key.len()).any(|window| window == key),
                        "key in job file"
                    );
                }
                if entry.path().extension().is_some_and(|e| e == "log") {
                    *logs += 1;
                }
            }
        }
    }
    let mut logs = 0;
    scan(&lab._dirs.path().join("jobs"), &mut logs);
    assert!(
        logs > 0,
        "must inspect raw recipe logs as well as checkout files"
    );
    let trace = trace.0.lock().unwrap().take().unwrap();
    assert!(!trace.is_empty(), "daemon tracing was not captured");
    for key in [
        b"dummy-reset-key-363".as_slice(),
        b"dummy-tunnel-key-363".as_slice(),
    ] {
        assert!(
            !trace.windows(key.len()).any(|window| window == key),
            "key in daemon tracing"
        );
    }
    let order = std::fs::read_to_string(fixtures.path().join("order")).unwrap();
    // The queue may retry another failed card while a new merge is queued.
    // Every tunnel, including those retries, must follow a successful reset.
    assert!(order.starts_with("reset\ntunnel\n"), "{order}");
    assert!(order.contains("reset\nreset\n"), "{order}");
    for pair in order.lines().collect::<Vec<_>>().windows(2) {
        if pair[1] == "tunnel" {
            assert_eq!(pair[0], "reset");
        }
    }
}

#[tokio::test]
async fn delivery_secret_config_refuses_plaintext_and_arbitrary_env_injection() {
    let lab = lab(1).await;
    for refs in [
        json!({"DOCKER_CI_KEY":"literal"}),
        json!({"MERGE_KEY":"@secret:vm"}),
        json!({"PATH":"@secret:vm"}),
    ] {
        lab.td
            .put(
                "/api/projects/p/delivery",
                json!({
                    "target":"p-box", "recipe":"true", "merge_secrets":refs
                }),
            )
            .await
            .err(422, "invalid");
    }
}

/// Review B1: the recipe commits in its clone and moves HEAD. The push
/// sends the merge commit that the prepare step recorded, never the
/// recipe's commit.
#[tokio::test]
async fn a_merge_pushes_the_recorded_commit_not_the_head_after_the_recipe() {
    let lab = lab(2).await;
    let before = git(&lab.origin, &["rev-parse", "next"]);
    let job = merge_with_recipe(
        &lab,
        "ter-m1",
        "test -f OK && echo evil > EVIL && git add EVIL \
         && git -c user.name=x -c user.email=x@x commit -q -m evil \
         && git checkout -q -B other && test \"$(git log -1 --format=%s)\" = evil",
    )
    .await;
    assert_eq!(job["state"], "passed", "{job:#}");
    let next = git(&lab.origin, &["rev-parse", "next"]);
    assert_ne!(next, before);
    assert_eq!(job["head"].as_str().unwrap(), next, "{job:#}");
    assert_eq!(
        git(&lab.origin, &["log", "-1", "--format=%s", "next"]),
        "Merge ter-m1 onto next"
    );
    let files = git(&lab.origin, &["ls-tree", "--name-only", "next"]);
    assert!(!files.lines().any(|f| f == "EVIL"), "{files}");
    let all = git(&lab.origin, &["log", "--all", "--format=%s"]);
    assert!(!all.lines().any(|l| l == "evil"), "{all}");
}

/// Review B2: the recipe plants `url.<x>.insteadOf` and `pushInsteadOf`
/// in its clone's `.git/config` to send the push (and the credential) to
/// another repo. The push does not read that config: `next` goes to the
/// project's repo, and the other repo gets nothing.
#[tokio::test]
async fn a_merge_push_ignores_the_config_that_the_recipe_wrote() {
    let lab = lab(2).await;
    let evil = lab._dirs.path().join("evil.git");
    std::fs::create_dir_all(&evil).unwrap();
    git(&evil, &["init", "-q", "--bare", "-b", "main"]);
    let (origin, evil_s) = (lab.origin.display().to_string(), evil.display().to_string());
    let recipe = format!(
        "test -f OK && git config 'url.{evil_s}.insteadOf' '{origin}' \
         && git config 'url.{evil_s}.pushInsteadOf' '{origin}' \
         && git config remote.pushDefault evil && git config remote.evil.url '{evil_s}'"
    );
    let job = merge_with_recipe(&lab, "ter-m2", &recipe).await;
    assert_eq!(job["state"], "passed", "{job:#}");
    assert_eq!(
        job["head"].as_str().unwrap(),
        git(&lab.origin, &["rev-parse", "next"])
    );
    assert_eq!(
        git(&evil, &["for-each-ref"]),
        "",
        "the other repo got a push"
    );
}

/// TER-103: the target holds no credential and no push repo. The recipe
/// of a merge finds no `.ssh` or `.push` dir beside its clone, no git
/// credential env, and no remote in its clone. `TERRARIUM_GIT_REF` is a
/// public build stamp and `TERRARIUM_GIT_TRAILERS` is public provenance,
/// not a credential. It deletes the job ref in
/// its clone, and the merge still pushes the recorded commit: the push
/// runs from the daemon's repo.
#[tokio::test]
async fn a_merge_recipe_finds_no_credential_and_cannot_touch_the_push() {
    let lab = lab(2).await;
    let before = git(&lab.origin, &["rev-parse", "next"]);
    let recipe = "test -f OK \
         && ! ls -a .. | grep -E '[.](ssh|push)$' \
         && ! env | grep -vE '^TERRARIUM_GIT_(REF|TRAILERS)=' | grep -E '^(TERRARIUM_GIT|GIT_SSH_COMMAND)' \
         && test -z \"$(git remote)\" \
         && test \"$(git rev-parse refs/job/merge)\" = \"$(git rev-parse HEAD)\" \
         && git update-ref -d refs/job/merge \
         && rm -rf .git/objects/pack";
    let job = merge_with_recipe(&lab, "ter-m3", recipe).await;
    assert_eq!(job["state"], "passed", "{job:#}");
    let next = git(&lab.origin, &["rev-parse", "next"]);
    assert_ne!(next, before);
    assert_eq!(job["head"].as_str().unwrap(), next, "{job:#}");
    assert_eq!(
        git(&lab.origin, &["log", "-1", "--format=%s", "next"]),
        "Merge ter-m3 onto next"
    );
    // The daemon keeps its repository, but removes the job's bundle.
    let state_jobs = lab.td.daemon.as_ref().unwrap().state.state_dir.join("jobs");
    let left: Vec<_> = std::fs::read_dir(state_jobs.join("p"))
        .map(|d| d.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert_eq!(left, [std::ffi::OsString::from("repos")]);
    let cached = state_jobs.join("p/repos/app.git");
    assert_eq!(git(&cached, &["rev-parse", &next]), next);
    git(&cached, &["update-ref", "refs/test/retained", &next]);
    lab.branch("ter-m4", "next", &[("SECOND", "yes\n")]);
    let card = lab.card("ter-m4", "ter-m4").await;
    lab.td
        .post_as(&lab.lead, &format!("/api/cards/{card}/merge"), json!({}))
        .await
        .ok();
    let second = lab
        .until_idle()
        .await
        .into_iter()
        .find(|j| j["card"] == card.as_str())
        .unwrap();
    assert_eq!(second["state"], "passed", "{second:#}");
    assert_eq!(git(&cached, &["rev-parse", "refs/test/retained"]), next);
    assert_eq!(
        git(&cached, &["rev-parse", "refs/job/merge"]),
        second["head"].as_str().unwrap()
    );
    let repos: Vec<_> = std::fs::read_dir(state_jobs.join("p/repos"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(repos, [std::ffi::OsString::from("app.git")]);
}

/// A crash can leave a lock or broken ref in the daemon's persistent
/// cache. The next job rebuilds it, fetches, and merges successfully.
#[tokio::test]
async fn the_next_job_recovers_a_damaged_daemon_repo() {
    let lab = lab(2).await;
    let first = merge_with_recipe(&lab, "ter-cache", "test -f OK").await;
    assert_eq!(first["state"], "passed", "{first:#}");
    let cached = lab
        .td
        .daemon
        .as_ref()
        .unwrap()
        .state
        .state_dir
        .join("jobs/p/repos/app.git");
    for (i, damage) in ["index.lock", "refs/job/broken"].iter().enumerate() {
        std::fs::write(cached.join(damage), "interrupted").unwrap();
        let branch = format!("ter-recover-{i}");
        lab.branch(&branch, "next", &[(&branch, "new\n")]);
        let card = lab.card(&branch, &branch).await;
        lab.td
            .post_as(&lab.lead, &format!("/api/cards/{card}/merge"), json!({}))
            .await
            .ok();
        let job = lab
            .until_idle()
            .await
            .into_iter()
            .find(|j| j["card"] == card.as_str())
            .unwrap();
        assert_eq!(job["state"], "passed", "{damage}: {job:#}");
        assert!(!cached.join(damage).exists());
        assert_eq!(
            git(&cached, &["rev-parse", "refs/job/merge"]),
            job["head"].as_str().unwrap()
        );
    }
}

/// TER-159: merges have their own lane. With 1 slot, a validate job queued
/// after a merge starts beside it and passes while the merge runs, and a
/// second merge waits for the first.
#[tokio::test]
async fn a_validation_passes_while_a_merge_runs() {
    let lab = lab(1).await;
    lab.td
        .put(
            "/api/projects/p/delivery",
            json!({"target": "p-box", "recipe": "test -f OK && sleep 2", "slots": 1,
                   "next_repos": ["app"],
                   "work_dir": lab._dirs.path().join("jobs").display().to_string()}),
        )
        .await
        .ok();
    lab.branch("ter-m1", "next", &[("OK", "1")]);
    let hv = lab.branch("ter-v", "next", &[("OK", "2")]);
    // Its own file: it merges onto the `next` of the first merge, which
    // holds `OK`.
    lab.branch("ter-m2", "next", &[("M2", "3")]);
    let cm1 = lab.card("m1", "ter-m1").await;
    let cv = lab.card("v", "ter-v").await;
    let cm2 = lab.card("m2", "ter-m2").await;
    let r = lab
        .td
        .post_as(&lab.lead, &format!("/api/cards/{cm1}/merge"), json!({}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["queue"]["slots"], 1, "{}", r.body);
    let r = lab
        .td
        .post_as(&lab.lead, &format!("/api/cards/{cm2}/merge"), json!({}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    // Behind the first merge, queued or running.
    let q = &r.body["queue"];
    assert_eq!(
        q["position"].as_i64().unwrap() + q["running"].as_i64().unwrap(),
        2,
        "{}",
        r.body
    );
    let r = lab
        .td
        .post_as(
            &lab.w1,
            &format!("/api/cards/{cv}/validate"),
            json!({"commit": hv}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    // The merges do not count against the one slot.
    assert_eq!(r.body["queue"]["position"], 1, "{}", r.body);
    assert_eq!(r.body["queue"]["running"], 0, "{}", r.body);
    let jobs = lab.until_idle().await;
    let of = |c: &str| jobs.iter().find(|j| j["card"] == c).unwrap().clone();
    let (m1, v, m2) = (of(&cm1), of(&cv), of(&cm2));
    for j in [&m1, &v, &m2] {
        assert_eq!(j["state"], "passed", "{jobs:#?}");
    }
    let t = |j: &Value, k: &str| j[k].as_str().unwrap().to_owned();
    // The validate job ran while a merge ran.
    let overlaps = |m: &Value| {
        t(&v, "started_at") < t(m, "ended_at") && t(m, "started_at") < t(&v, "ended_at")
    };
    assert!(overlaps(&m1) || overlaps(&m2), "{jobs:#?}");
    // One merge at a time, in order.
    assert!(t(&m2, "started_at") >= t(&m1, "ended_at"), "{jobs:#?}");
}

// ------------------------------------------------------------- TER-115

/// Is `pid` a live process (not gone, not a zombie)?
fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .is_ok_and(|s| !s.rsplit(") ").next().unwrap_or("").starts_with('Z'))
}

/// The PIDs in `file`, one per line, once there are at least `n`.
async fn pids(file: &Path, n: usize) -> Vec<u32> {
    let start = Instant::now();
    loop {
        let p: Vec<u32> = std::fs::read_to_string(file)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect();
        if p.len() >= n {
            return p;
        }
        assert!(
            start.elapsed() < DEADLINE,
            "no {n} pids in {}",
            file.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A fake `docker` whose `exec` runs the command in a new session on
/// this machine, as runc does in a container: a kill of the `docker`
/// client (the host agent's timeout and kill) does not reach it.
fn fake_docker(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let docker = dir.join("docker");
    std::fs::write(
        &docker,
        r#"#!/bin/sh
case "$1" in
  inspect) echo '[{"Id":"abc","Name":"/terrarium-p-dbox","State":{"Status":"running"}}]' ;;
  run) echo abc ;;
  exec) shift
    while [ $# -gt 0 ]; do case "$1" in -i|-t) shift ;; -w) cd "$2" || exit 1; shift 2 ;; -e|-u) shift 2 ;; *) break ;; esac; done
    shift
    # Recovery tests can hold kills before execution, rather than race
    # the five-second TERM grace on a loaded host.
    gate="${0%/*}/kill-gate"
    if [ "$4" = terrarium-kill ] && [ -d "$gate" ]; then
      touch "$gate/${5##*/}"
      while [ -d "$gate" ]; do sleep 0.02; done
    fi
    # An async list gets /dev/null on stdin: keep the real one (`-i`, the
    # bundle of a job) on fd 3 and hand it on.
    exec 3<&0
    ( exec setsid "$@" <&3 3<&- ) &
    wait $! ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
    docker
}

/// A lab whose queue runs on the docker target `p-dbox` (fake docker), with
/// a recipe that starts a child, ignores `TERM` (the child too), and
/// appends the child's PID to the returned file.
async fn docker_lab(slots: u32, timeout_secs: u64) -> (Lab, tempfile::TempDir, PathBuf) {
    docker_lab_flow(slots, timeout_secs, "code").await
}

/// [`docker_lab`] with the board flow `flow` (TER-170).
async fn docker_lab_flow(
    slots: u32,
    timeout_secs: u64,
    flow: &'static str,
) -> (Lab, tempfile::TempDir, PathBuf) {
    let bins = tempfile::tempdir().unwrap();
    let docker = fake_docker(bins.path());
    let lab = lab_impl(slots, Some(&docker), flow).await;
    let r = lab
        .td
        .post(
            "/api/targets",
            json!({"name": "p-dbox", "kind": "docker", "host": "lab1", "image": "alpine:3",
                   "project": "p"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let child_file = bins.path().join("children");
    let recipe = format!(
        "printf 'test interrupted ... FAILED\\npanic output before kill\\n'; trap '' TERM; sleep 300 & echo $! >> '{}'; wait",
        child_file.display()
    );
    let work_dir = bins.path().join("jobs");
    let r = lab
        .td
        .put(
            "/api/projects/p/delivery",
            json!({"target": "p-dbox", "recipe": recipe, "slots": slots,
                   "timeout_secs": timeout_secs,
                   "work_dir": work_dir.display().to_string()}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    (lab, bins, child_file)
}

/// Poll job `id` until its state is not `state`; return it.
async fn until_not(lab: &Lab, id: &str, state: &str) -> Value {
    let start = Instant::now();
    loop {
        let j = lab.td.get(&format!("/api/jobs/{id}")).await.ok().clone();
        if j["state"] != state {
            return j;
        }
        assert!(start.elapsed() < DEADLINE, "job {id} stays {state}: {j}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// TER-115: a recipe on a docker target (its command in its own session,
/// as in a container) starts a child that ignores `TERM`, and the job
/// hits its timeout. The job ends only after the kill on the target: when
/// it shows as ended, no process of it is left. The rerun does the same.
#[tokio::test]
async fn a_job_timeout_ends_every_process_of_the_recipe() {
    let (lab, _bins, children) = docker_lab(1, 1).await;
    let _ticks = QueueTicks::start(&lab);
    let h = lab.branch("ter-t1", "next", &[("OK", "1")]);
    let c = lab.card("t1", "ter-t1").await;
    let r = lab
        .td
        .post_as(
            &lab.w1,
            &format!("/api/cards/{c}/validate"),
            json!({"commit": h}),
        )
        .await;
    let j1 = r.ok()["job"]["id"].as_str().unwrap().to_owned();
    let first = pids(&children, 1).await[0];
    assert!(alive(first));
    let j = until_not(&lab, &j1, "running").await;
    assert!(!alive(first), "the child {first} outlived its job: {j}");
    assert_eq!(j["state"], "failed", "{j}");
    assert_eq!(j["outcome"], "red", "{j}");
    let tail = j["tail"].as_str().unwrap_or("");
    assert!(tail.contains("timeout"), "{j}");
    assert!(tail.contains("killed on the target"), "{j}");
    assert!(tail.contains("test interrupted ... FAILED"), "{j}");
    assert!(tail.contains("panic output before kill"), "{j}");
    // The rerun times out the same way.
    let jobs = lab.until_idle().await;
    assert_eq!(jobs.len(), 2, "{jobs:?}");
    for p in pids(&children, 2).await {
        assert!(!alive(p), "the child {p} outlived its job");
    }
}

/// TER-115: a cancel of a running job ends every process of its recipe on
/// the target, and the next job starts only after they are gone.
#[tokio::test]
async fn a_cancel_ends_every_process_before_the_next_job_starts() {
    let (lab, _bins, children) = docker_lab(1, 600).await;
    let h1 = lab.branch("ter-k1", "next", &[("OK", "1")]);
    let h2 = lab.branch("ter-k2", "next", &[("OK", "2")]);
    let c1 = lab.card("k1", "ter-k1").await;
    let c2 = lab.card("k2", "ter-k2").await;
    let mut ids = Vec::new();
    for (c, h) in [(&c1, &h1), (&c2, &h2)] {
        let r = lab
            .td
            .post_as(
                &lab.w1,
                &format!("/api/cards/{c}/validate"),
                json!({"commit": h}),
            )
            .await;
        ids.push(r.ok()["job"]["id"].as_str().unwrap().to_owned());
    }
    let first = pids(&children, 1).await[0];
    assert!(alive(first));
    let r = lab.td.delete(&format!("/api/jobs/{}", ids[0])).await;
    assert_eq!(r.ok()["state"], "cancelled");
    let j2 = until_not(&lab, &ids[1], "queued").await;
    assert!(
        !alive(first),
        "job 2 started while the child {first} of job 1 ran: {j2}"
    );
    // Job 2's recipe runs; cancel it too.
    let all = pids(&children, 2).await;
    let r = lab.td.delete(&format!("/api/jobs/{}", ids[1])).await;
    assert_eq!(r.ok()["state"], "cancelled");
    for p in all {
        let start = Instant::now();
        while alive(p) {
            assert!(start.elapsed() < DEADLINE, "the child {p} outlived its job");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    // Cancellation keeps its state but receives the final retained output
    // once the killed recipe has drained and the separate log read ends.
    for id in ids {
        let start = Instant::now();
        loop {
            let j = lab.td.get(&format!("/api/jobs/{id}")).await.ok().clone();
            assert_eq!(j["state"], "cancelled");
            let tail = j["tail"].as_str().unwrap_or("");
            if tail.contains("panic output before kill") {
                assert!(tail.contains("test interrupted ... FAILED"), "{j}");
                assert!(tail.contains("Full log on target p-dbox:"), "{j}");
                break;
            }
            assert!(start.elapsed() < DEADLINE, "cancelled log missing: {j}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// TER-115 review (round 2, minor 2): a merge job that gets a cancel
/// between two steps starts no more steps, so it does not push `next`.
/// A fake `ionice` holds the first ionice probe (the step before the
/// prepare step) until the kill. The cancel comes during the probe, so
/// the kill ends only the probe, and only the killed() check keeps the
/// prepare, recipe, collect, and push steps from running. A validate job
/// behind it (one slot) starts only after the merge job's run ended.
#[tokio::test]
async fn a_cancelled_merge_job_does_not_push() {
    use std::os::unix::fs::PermissionsExt;
    let bins = tempfile::tempdir().unwrap();
    let docker = fake_docker(bins.path());
    let script = std::fs::read_to_string(&docker).unwrap().replacen(
        "#!/bin/sh\n",
        &format!(
            "#!/bin/sh\nPATH='{}':\"$PATH\"; export PATH\n",
            bins.path().display()
        ),
        1,
    );
    std::fs::write(&docker, script).unwrap();
    let probe = bins.path().join("probe");
    let ionice = bins.path().join("ionice");
    std::fs::write(
        &ionice,
        format!(
            "#!/bin/sh\nif [ ! -e '{p}' ]; then echo $$ > '{p}'; exec sleep 300; fi\nshift\nexec \"$@\"\n",
            p = probe.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&ionice, std::fs::Permissions::from_mode(0o755)).unwrap();
    let lab = lab_with(1, Some(&docker)).await;
    // A target name of its own: the ionice answer is kept per target name
    // in the process, and this test needs the probe to run.
    let r = lab
        .td
        .post(
            "/api/targets",
            json!({"name": "p-ibox", "kind": "docker", "host": "lab1", "image": "alpine:3",
                   "project": "p"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let ran = bins.path().join("ran");
    let work_dir = bins.path().join("jobs");
    // The tree is the slot's (TER-190): the recipe logs its job ID.
    let recipe = format!(
        "echo \"$TERRARIUM_JOB_ID\" >> '{}'; test -f OK",
        ran.display()
    );
    let r = lab
        .td
        .put(
            "/api/projects/p/delivery",
            json!({"target": "p-ibox", "slots": 1, "next_repos": ["app"],
                   "recipe": recipe,
                   "work_dir": work_dir.display().to_string()}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let before = git(&lab.origin, &["rev-parse", "next"]);
    lab.branch("ter-mc", "next", &[("OK", "yes\n")]);
    let card = lab.card("mc", "ter-mc").await;
    let r = lab
        .td
        .post_as(&lab.lead, &format!("/api/cards/{card}/merge"), json!({}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let job = r.body["job"]["id"].as_str().unwrap().to_owned();
    let held = pids(&probe, 1).await[0];
    assert!(alive(held));
    let r = lab.td.delete(&format!("/api/jobs/{job}")).await;
    assert_eq!(r.ok()["state"], "cancelled");
    let h2 = lab.branch("ter-mv", "next", &[("OK", "yes\n")]);
    let c2 = lab.card("mv", "ter-mv").await;
    let r = lab
        .td
        .post_as(
            &lab.w1,
            &format!("/api/cards/{c2}/validate"),
            json!({"commit": h2}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let v = r.body["job"]["id"].as_str().unwrap().to_owned();
    let jobs = lab.until_idle().await;
    let of = |id: &str| jobs.iter().find(|j| j["id"] == id).unwrap().clone();
    assert_eq!(of(&job)["state"], "cancelled", "{jobs:#?}");
    assert_eq!(of(&v)["state"], "passed", "{jobs:#?}");
    assert!(!alive(held), "the probe {held} outlived the kill");
    let log = std::fs::read_to_string(&ran).unwrap_or_default();
    assert!(
        !log.contains(&job),
        "the cancelled job ran its recipe: {log}"
    );
    assert!(log.contains(&v), "the validate job did not run: {log}");
    assert_eq!(git(&lab.origin, &["rev-parse", "next"]), before);
}

/// Hold each job of project `p` until the test writes the returned gate
/// file, then run the lab recipe. A test that checks the card while its job
/// is queued or running holds the job: with `board_flow = rules` the rule
/// engine can lag behind the job, so a fast job can pass before the test
/// sees the card in `validating` (TER-170 merge fix).
async fn hold_jobs(lab: &Lab) -> PathBuf {
    let gate = lab._dirs.path().join("gate");
    set_recipe(
        lab,
        &format!(
            "while ! test -f '{}'; do sleep 0.05; done; test -f OK",
            gate.display()
        ),
    )
    .await;
    gate
}

/// Set the recipe of project `p`, with the lab's work dir.
async fn set_recipe(lab: &Lab, recipe: &str) {
    lab.td
        .put(
            "/api/projects/p/delivery",
            json!({"target": "p-box", "recipe": recipe, "slots": 2, "next_repos": ["app"],
                   "work_dir": lab._dirs.path().join("jobs").display().to_string()}),
        )
        .await
        .ok();
}

/// The subjects of the merges on `next` of the remote, oldest first.
fn next_merges(lab: &Lab) -> Vec<String> {
    let log = git(
        &lab.origin,
        &["log", "--first-parent", "--reverse", "--format=%s", "next"],
    );
    log.lines()
        .filter(|l| l.starts_with("Merge "))
        .map(str::to_owned)
        .collect()
}

both_flows!(a_green_batch_of_three_merges_in_one_job);
/// TER-112: one merge job takes three cards, merges them onto `next` in
/// the given order, validates once, pushes `next`, and moves each card to
/// `accepted`.
async fn a_green_batch_of_three_merges_in_one_job(flow: &'static str) {
    let lab = lab_flow(2, flow).await;
    set_recipe(
        &lab,
        "test -f OK && test -f g1 && test -f g2 && test -f g3 && sleep 1",
    )
    .await;
    let mut heads = Vec::new();
    let mut cards = Vec::new();
    for n in ["g1", "g2", "g3"] {
        let b = format!("ter-{n}");
        heads.push(lab.branch(&b, "next", &[("OK", "yes\n"), (n, n)]));
        cards.push(lab.card(n, &b).await);
    }
    // Only an admin queues a batch, and a batch is checked.
    lab.td
        .post_as(&lab.w1, "/api/merges", json!({"cards": cards}))
        .await
        .err(403, "forbidden");
    lab.td
        .post_as(&lab.lead, "/api/merges", json!({"cards": []}))
        .await
        .err(422, "invalid");
    lab.td
        .post_as(
            &lab.lead,
            "/api/merges",
            json!({"cards": [cards[0], cards[0]]}),
        )
        .await
        .err(422, "invalid");
    // The first card is w1's: w1 still may not cancel the batch (review
    // F3), as it holds the other cards too.
    lab.td
        .patch(
            &format!("/api/cards/{}", cards[0]),
            json!({"add_labels": ["by:w1"]}),
        )
        .await
        .ok();
    let r = lab
        .td
        .post_as(&lab.lead, "/api/merges", json!({"cards": cards}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["job"]["kind"], "merge");
    assert_eq!(r.body["job"]["card"], cards[0].as_str());
    assert_eq!(r.body["job"]["by_agent"], "w1", "{}", r.body);
    let job_id = r.body["job"]["id"].as_str().unwrap().to_owned();
    lab.td
        .req(
            hyper::Method::DELETE,
            &format!("/api/jobs/{job_id}"),
            None,
            Some(&lab.w1),
        )
        .await
        .err(403, "forbidden");
    let batch: Vec<&str> = r.body["job"]["batch"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["card"].as_str().unwrap())
        .collect();
    assert_eq!(batch, cards.iter().map(String::as_str).collect::<Vec<_>>());
    // A card of the open batch takes no other job.
    lab.td
        .post_as(
            &lab.lead,
            &format!("/api/cards/{}/merge", cards[1]),
            json!({}),
        )
        .await
        .err(409, "conflict");
    // Each card shows the job, unless the job has ended already.
    for c in &cards {
        lab.until_card(c, "a job label", |card| {
            labels(card).iter().any(|l| l.starts_with("validation:"))
                || card["column"] == "accepted"
        })
        .await;
    }
    for c in &cards {
        let card = lab
            .until_card(c, "accepted", |c| c["column"] == "accepted")
            .await;
        let l = labels(&card);
        assert!(!l.iter().any(|l| l.starts_with("validation:")), "{l:?}");
        assert!(comments(&card).contains("batch of 3"), "{card}");
    }
    let jobs = lab.until_idle().await;
    assert_eq!(jobs.len(), 1, "one job for the batch: {jobs:#?}");
    assert_eq!(jobs[0]["state"], "passed");
    let next = git(&lab.origin, &["rev-parse", "next"]);
    assert_eq!(jobs[0]["head"].as_str().unwrap(), next);
    for h in &heads {
        git(&lab.origin, &["merge-base", "--is-ancestor", h, "next"]);
    }
    assert_eq!(
        next_merges(&lab),
        [
            "Merge ter-g1 onto next",
            "Merge ter-g2 onto next",
            "Merge ter-g3 onto next"
        ]
    );
    lab.settle().await;
    assert_eq!(lab.failed_rule_runs().await, Vec::<Value>::new());
}

/// TER-112, with `board_flow = code`: a red batch splits in halves until
/// it finds the failing card. (The rule pack gives a red group fix cards
/// first: [`a_red_batch_gets_fix_cards_then_splits_with_the_rule_pack`].)
/// The failing card goes to the top of `ready` with the failure block,
/// and the other cards merge, in order. The bad card is in the left half,
/// so the left half splits again and its halves must still run before
/// the right half (review F1, F2).
#[tokio::test]
async fn a_red_batch_finds_the_failing_card_and_the_others_merge() {
    let lab = lab(2).await;
    set_recipe(&lab, "test -f OK && test ! -f BAD").await;
    let h1 = lab.branch("ter-r1", "next", &[("OK", "yes\n"), ("r1", "1")]);
    let h2 = lab.branch("ter-r2", "next", &[("OK", "yes\n"), ("BAD", "2")]);
    let h3 = lab.branch("ter-r3", "next", &[("OK", "yes\n"), ("r3", "3")]);
    let h4 = lab.branch("ter-r4", "next", &[("OK", "yes\n"), ("r4", "4")]);
    let c1 = lab.card("r1", "ter-r1").await;
    let c2 = lab.card("r2", "ter-r2").await;
    let c3 = lab.card("r3", "ter-r3").await;
    let c4 = lab.card("r4", "ter-r4").await;
    let r = lab
        .td
        .post_as(&lab.lead, "/api/merges", json!({"cards": [c1, c2, c3, c4]}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let bad = lab
        .until_card(&c2, "ready", |c| c["column"] == "ready")
        .await;
    let body = bad["body"].as_str().unwrap();
    assert!(body.starts_with("Validation failed: "), "{body}");
    assert!(body.contains("outcome red"), "{body}");
    assert!(body.contains("ter-r2"), "{body}");
    assert!(body.ends_with("the card body"), "{body}");
    assert!(bad["assignee"].is_null(), "{bad}");
    for c in [&c1, &c3, &c4] {
        lab.until_card(c, "accepted", |c| c["column"] == "accepted")
            .await;
    }
    let jobs = lab.until_idle().await;
    // [r1 r2 r3 r4] red; [r1 r2] red, then [r1] green and [r2] red (no
    // rerun: its batch was red); then [r3 r4] green.
    assert_eq!(jobs.len(), 5, "{jobs:#?}");
    let state_of = |path: &str| -> String {
        let j = jobs
            .iter()
            .find(|j| j["split_path"].as_str().unwrap_or("") == path)
            .unwrap_or_else(|| panic!("no job at {path:?}: {jobs:#?}"));
        j["state"].as_str().unwrap().to_owned()
    };
    assert_eq!(state_of(""), "failed");
    assert_eq!(state_of("0"), "failed");
    assert_eq!(state_of("00"), "passed");
    assert_eq!(state_of("01"), "failed");
    assert_eq!(state_of("1"), "passed");
    assert!(
        jobs.iter()
            .filter(|j| j["outcome"] == "red")
            .all(|j| j["attempt"] == 1),
        "a card split out alone is not rerun: {jobs:#?}"
    );
    for h in [&h1, &h3, &h4] {
        git(&lab.origin, &["merge-base", "--is-ancestor", h, "next"]);
    }
    let out = std::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", &h2, "next"])
        .current_dir(&lab.origin)
        .status()
        .unwrap();
    assert!(!out.success(), "the red card is not on next");
    assert_eq!(
        next_merges(&lab),
        [
            "Merge ter-r1 onto next",
            "Merge ter-r3 onto next",
            "Merge ter-r4 onto next"
        ]
    );
    let c1v = lab.card_of(&c1).await;
    assert!(comments(&c1v).contains("The batch splits"), "{c1v}");
}

/// The cards of project `p` with the label `label`.
async fn cards_with_label(lab: &Lab, label: &str) -> Vec<Value> {
    lab.td.get("/api/projects/p/board").await.ok()["cards"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| labels(c).iter().any(|l| l == label))
        .cloned()
        .collect()
}

/// Wait until job `id` is in a final state; returns it.
async fn until_ended(lab: &Lab, id: &str) -> Value {
    let start = Instant::now();
    loop {
        let j = lab.td.get(&format!("/api/jobs/{id}")).await.ok().clone();
        if ["passed", "failed", "cancelled"].contains(&j["state"].as_str().unwrap_or("")) {
            return j;
        }
        assert!(start.elapsed() < DEADLINE, "job {id} never ended: {j}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Poll job `id` until it runs. This is a precondition of the next
/// step, not a timing check: under load the start can wait for the host
/// or its admission (TER-160, TER-383), and a second job posted first
/// would take its place in the queue.
async fn until_running(lab: &Lab, id: &str) {
    let start = Instant::now();
    loop {
        let j = lab.td.get(&format!("/api/jobs/{id}")).await.ok().clone();
        if j["state"] == "running" {
            return;
        }
        assert!(start.elapsed() < DEADLINE, "job {id} never started: {j}");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

/// TER-170, MEM-84: with the rule pack a red merge group gets a fix card
/// (in `ready`, linked to its cards, with the failure block) and no split;
/// the lead queues the group again. After three fix cards the fourth red
/// splits the group, and the bisection finds the failing card as the
/// coded flow does.
#[tokio::test]
async fn a_red_batch_gets_fix_cards_then_splits_with_the_rule_pack() {
    let lab = lab_flow(2, "rules").await;
    set_recipe(&lab, "test -f OK && test ! -f BAD").await;
    let h1 = lab.branch("ter-r1", "next", &[("OK", "yes\n"), ("r1", "1")]);
    lab.branch("ter-r2", "next", &[("OK", "yes\n"), ("BAD", "2")]);
    let h3 = lab.branch("ter-r3", "next", &[("OK", "yes\n"), ("r3", "3")]);
    let h4 = lab.branch("ter-r4", "next", &[("OK", "yes\n"), ("r4", "4")]);
    let c1 = lab.card("r1", "ter-r1").await;
    let c2 = lab.card("r2", "ter-r2").await;
    let c3 = lab.card("r3", "ter-r3").await;
    let c4 = lab.card("r4", "ter-r4").await;
    let group = [c1.clone(), c2.clone(), c3.clone(), c4.clone()];
    for round in 1..=4 {
        let r = lab
            .td
            .post_as(&lab.lead, "/api/merges", json!({"cards": group}))
            .await;
        assert_eq!(r.status, 201, "round {round}: {}", r.body);
        let job = r.body["job"]["id"].as_str().unwrap().to_owned();
        let j = until_ended(&lab, &job).await;
        assert_eq!(j["outcome"], "red", "{j}");
        lab.settle().await;
        if round == 4 {
            break;
        }
        // A fix card for the job, and no split.
        let fix = cards_with_label(&lab, &format!("merge-fix:{job}")).await;
        assert_eq!(fix.len(), 1, "round {round}: {fix:?}");
        assert_eq!(fix[0]["column"], "ready", "{}", fix[0]);
        let fix = lab.card_of(fix[0]["id"].as_str().unwrap()).await;
        let mut links: Vec<&str> = fix["links"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|l| l["kind"] == "card")
            .map(|l| l["ref"].as_str().unwrap())
            .collect();
        links.sort_unstable();
        let mut want: Vec<&str> = group.iter().map(String::as_str).collect();
        want.sort_unstable();
        assert_eq!(links, want);
        assert!(
            fix["body"].as_str().unwrap().contains("Validation failed"),
            "{fix}"
        );
        let jobs = lab.jobs().await;
        assert_eq!(jobs.len(), round, "no split, no rerun: {jobs:#?}");
        for c in &group {
            let card = lab.card_of(c).await;
            assert_eq!(card["column"], "backlog", "{card}");
            assert!(
                !labels(&card).iter().any(|l| l.starts_with("validation:")),
                "{card}"
            );
            assert!(
                comments(&card).contains(&format!("fix card (label merge-fix:{job})")),
                "{card}"
            );
        }
    }
    // The fourth red splits: the bisection finds r2.
    let bad = lab
        .until_card(&c2, "ready", |c| c["column"] == "ready")
        .await;
    let body = bad["body"].as_str().unwrap();
    assert!(body.starts_with("Validation failed: "), "{body}");
    assert!(body.contains("outcome red"), "{body}");
    assert!(
        comments(&bad).contains("failed (red alone after its batch was red)"),
        "{bad}"
    );
    for c in [&c1, &c3, &c4] {
        lab.until_card(c, "accepted", |c| c["column"] == "accepted")
            .await;
    }
    let jobs = lab.until_idle().await;
    // Four red runs of the group; then [r1 r2] red, [r1] green, [r2]
    // red, and [r3 r4] green.
    assert_eq!(jobs.len(), 8, "{jobs:#?}");
    lab.settle().await;
    let fixes: usize = {
        let board = lab.td.get("/api/projects/p/board").await.ok().clone();
        board["cards"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| labels(c).iter().any(|l| l.starts_with("merge-fix:")))
            .count()
    };
    assert_eq!(fixes, 3, "three fix cards, then the split");
    for h in [&h1, &h3, &h4] {
        git(&lab.origin, &["merge-base", "--is-ancestor", h, "next"]);
    }
    // The same merges as the coded flow. Their order can differ: the rule
    // splits the left half a moment after the job ends, and the queue may
    // start the right half first (the rule engine runs after the event).
    let mut merges = next_merges(&lab);
    merges.sort();
    assert_eq!(
        merges,
        [
            "Merge ter-r1 onto next",
            "Merge ter-r3 onto next",
            "Merge ter-r4 onto next"
        ]
    );
    assert!(comments(&lab.card_of(&c1).await).contains("The batch splits"));
    assert_eq!(lab.failed_rule_runs().await, Vec::<Value>::new());
}

/// One scenario of the card flow on a fresh lab with `flow`, and the
/// board, the jobs, and the mails to the worker after it, with the IDs,
/// the commits, the times, and the paths made neutral. Each step waits
/// for the one before, except for the successful halves of the final
/// split batch, whose relative completion order is not defined.
async fn flow_scenario(flow: &'static str) -> Value {
    let lab = lab_flow(2, flow).await;
    set_recipe(&lab, "test -f OK && { test ! -f SLOW || sleep 30; }").await;
    let first = lab
        .td
        .post(
            "/api/projects/p/cards",
            json!({"title": "first in ready", "column": "ready"}),
        )
        .await
        .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let validate = |id: String, head: String| {
        let lab = &lab;
        async move {
            let r = lab
                .td
                .post_as(
                    &lab.w1,
                    &format!("/api/cards/{id}/validate"),
                    json!({"commit": head}),
                )
                .await;
            assert_eq!(r.status, 201, "{}", r.body);
            r.body["job"]["id"].as_str().unwrap().to_owned()
        }
    };
    // A green validate, then a red one (it runs twice).
    let head = lab.branch("ter-a", "next", &[("OK", "yes\n")]);
    let a = lab.card("green", "ter-a").await;
    validate(a.clone(), head).await;
    lab.until_card(&a, "review", |c| c["column"] == "review")
        .await;
    let head = lab.branch("ter-b", "next", &[("other", "x\n")]);
    let b = lab.card("red", "ter-b").await;
    validate(b.clone(), head).await;
    lab.until_card(&b, "ready", |c| c["column"] == "ready")
        .await;
    lab.until_idle().await;
    lab.settle().await;
    // A green merge, then a merge that conflicts.
    lab.branch("ter-c", "next", &[("OK", "yes\n"), ("c", "c")]);
    let c = lab.card("merge", "ter-c").await;
    let r = lab
        .td
        .post_as(&lab.lead, &format!("/api/cards/{c}/merge"), json!({}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    lab.until_card(&c, "accepted", |c| c["column"] == "accepted")
        .await;
    lab.until_idle().await;
    lab.settle().await;
    lab.branch("ter-d1", "next", &[("README", "d1\n"), ("OK", "y\n")]);
    git(&lab.work, &["push", "-q", "origin", "ter-d1:next"]);
    lab.branch("ter-d2", "main", &[("README", "d2\n"), ("OK", "y\n")]);
    let d = lab.card("conflict", "ter-d2").await;
    let r = lab
        .td
        .post_as(&lab.lead, &format!("/api/cards/{d}/merge"), json!({}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    lab.until_card(&d, "ready", |c| c["column"] == "ready")
        .await;
    lab.until_idle().await;
    lab.settle().await;
    // A validate that is cancelled while it runs.
    let head = lab.branch("ter-e", "next", &[("OK", "yes\n"), ("SLOW", "1")]);
    let e = lab.card("cancel", "ter-e").await;
    let job = validate(e.clone(), head).await;
    let start = Instant::now();
    while lab.td.get(&format!("/api/jobs/{job}")).await.ok()["state"] != "running" {
        assert!(start.elapsed() < DEADLINE, "the job never started");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    lab.settle().await;
    lab.td.delete(&format!("/api/jobs/{job}")).await.ok();
    lab.until_idle().await;
    lab.settle().await;
    // A batch of three: the second conflicts with the first.
    lab.branch("ter-f1", "next", &[("README", "f1\n"), ("OK", "y\n")]);
    lab.branch("ter-f2", "next", &[("README", "f2\n"), ("OK", "y\n")]);
    lab.branch("ter-f3", "next", &[("f3", "3"), ("OK", "y\n")]);
    let f1 = lab.card("f1", "ter-f1").await;
    let f2 = lab.card("f2", "ter-f2").await;
    let f3 = lab.card("f3", "ter-f3").await;
    let r = lab
        .td
        .post_as(&lab.lead, "/api/merges", json!({"cards": [f1, f2, f3]}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    lab.until_card(&f2, "ready", |c| c["column"] == "ready")
        .await;
    lab.until_card(&f3, "accepted", |c| c["column"] == "accepted")
        .await;
    lab.until_idle().await;
    // The mail to the worker of the red card.
    let start = Instant::now();
    while lab.td.mail_of(&lab.w1).await.is_empty() {
        assert!(start.elapsed() < DEADLINE, "no mail to w1");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    lab.settle().await;
    assert_eq!(lab.failed_rule_runs().await, Vec::<Value>::new(), "{flow}");

    let dir = lab._dirs.path().display().to_string();
    let ulid = regex::Regex::new(r"\b[0-9A-HJKMNP-TV-Z]{26}\b").unwrap();
    let sha = regex::Regex::new(r"\b[0-9a-f]{7,40}\b").unwrap();
    let time = regex::Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})")
        .unwrap();
    // Rules allocate group headers from the card sequence. Compare the
    // original work cards by scenario identity, not their sequence gaps.
    let work_cards = [&first, &a, &b, &c, &d, &e, &f1, &f2, &f3];
    let card_id = regex::Regex::new(r"\bP-\d+\b").unwrap();
    let neutral = |text: &str| -> String {
        let t = text.replace(&dir, "<DIR>");
        let t = time.replace_all(&t, "<TIME>");
        let t = ulid.replace_all(&t, "<ID>");
        let t = sha.replace_all(&t, "<SHA>");
        card_id
            .replace_all(&t, |caps: &regex::Captures<'_>| {
                work_cards
                    .iter()
                    .position(|id| id.as_str() == &caps[0])
                    .map(|i| format!("<CARD-{i}>"))
                    .unwrap_or_else(|| caps[0].to_owned())
            })
            .into_owned()
    };
    let mut board = lab.td.get("/api/projects/p/board").await.ok().clone();
    // The split halves can finish in either order: in rules mode a
    // deferred split can let the right half start before the left (see
    // the batch-splitting test above). Compare that cohort in card-ID
    // order, but retain its positions relative to every other card.
    // In particular, neither card may overtake the earlier green merge.
    let board_cards = board["cards"].as_array_mut().unwrap();
    board_cards.retain(|card| card["kind"] != "group");
    let cohort: Vec<usize> = board_cards
        .iter()
        .enumerate()
        .filter(|(_, card)| card["id"] == f1 || card["id"] == f3)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(cohort.len(), 2);
    for &i in &cohort {
        assert_eq!(board_cards[i]["column"], "accepted");
    }
    if board_cards[cohort[0]]["id"] != f1 {
        board_cards.swap(cohort[0], cohort[1]);
    }
    let accepted: Vec<&str> = board_cards
        .iter()
        .filter(|card| card["column"] == "accepted")
        .map(|card| card["id"].as_str().unwrap())
        .collect();
    assert_eq!(accepted, [&c, &f1, &f3]);
    let mut cards = Vec::new();
    for id in [&first, &a, &b, &c, &d, &e, &f1, &f2, &f3] {
        let card = lab.card_of(id).await;
        let position = board["cards"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|x| x["column"] == card["column"])
            .position(|x| x["id"] == id.as_str());
        let mut l = labels(&card);
        l.sort();
        let notes: Vec<String> = card["comments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| neutral(c["body"].as_str().unwrap_or("")))
            .collect();
        cards.push(json!({
            "id": neutral(id), "column": card["column"], "position": position, "labels": l,
            "assignee": card["assignee_name"], "body": neutral(card["body"].as_str().unwrap_or("")),
            "comments": notes,
        }));
    }
    let mut jobs: Vec<Value> = lab
        .jobs()
        .await
        .iter()
        .map(|j| {
            json!({"card": neutral(j["card"].as_str().unwrap()), "kind": j["kind"], "state": j["state"],
                   "outcome": j["outcome"], "attempt": j["attempt"],
                   "split_path": j["split_path"]})
        })
        .collect();
    jobs.sort_by_key(|j| j.to_string());
    let mails: Vec<Value> = lab
        .td
        .mail_of(&lab.w1)
        .await
        .iter()
        .map(|m| {
            json!({"subject": neutral(m["subject"].as_str().unwrap_or("")),
                   "body": neutral(m["body"].as_str().unwrap_or(""))})
        })
        .collect();
    json!({"cards": cards, "jobs": jobs, "mails": mails})
}

/// TER-170 acceptance: one scenario (a green and a red validate, a green
/// and a conflicting merge, a cancel while it runs, a batch with a
/// conflict) leaves the same board with `board_flow = code` and with
/// `board_flow = rules`: the columns, the positions (with the concurrent
/// split-batch cohort in card-ID order), the labels, the
/// assignees, the bodies, the comment texts in order, the jobs, and the
/// mails to the worker (subject and body). Not compared, by design: the
/// author of a comment (`system` against `rule:p/<rule>`) and the sender
/// of the mail (`system` against `project:p`).
#[tokio::test]
async fn the_two_flows_leave_the_same_board() {
    let code = flow_scenario("code").await;
    let rules = flow_scenario("rules").await;
    for (c, r) in code["cards"]
        .as_array()
        .unwrap()
        .iter()
        .zip(rules["cards"].as_array().unwrap())
    {
        assert_eq!(c, r, "the card differs:\ncode:  {c:#}\nrules: {r:#}");
    }
    assert_eq!(code["jobs"], rules["jobs"]);
    assert_eq!(code["mails"], rules["mails"]);
    assert!(!code["mails"].as_array().unwrap().is_empty());
    assert_eq!(code, rules);
}

both_flows!(a_conflict_in_a_batch_drops_only_that_card);
/// TER-112: a card of a batch that conflicts with the cards before it
/// leaves the batch at once (a conflict is never resolved or rerun); the
/// others validate and merge in the same job.
async fn a_conflict_in_a_batch_drops_only_that_card(flow: &'static str) {
    let lab = lab_flow(2, flow).await;
    lab.branch("ter-k1", "next", &[("README", "k1\n"), ("OK", "y\n")]);
    lab.branch("ter-k2", "next", &[("README", "k2\n"), ("OK", "y\n")]);
    let h3 = lab.branch("ter-k3", "next", &[("k3", "3"), ("OK", "y\n")]);
    let c1 = lab.card("k1", "ter-k1").await;
    let c2 = lab.card("k2", "ter-k2").await;
    let c3 = lab.card("k3", "ter-k3").await;
    let r = lab
        .td
        .post_as(&lab.lead, "/api/merges", json!({"cards": [c1, c2, c3]}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let k2 = lab
        .until_card(&c2, "ready", |c| c["column"] == "ready")
        .await;
    let body = k2["body"].as_str().unwrap();
    assert!(body.starts_with("Validation failed: "), "{body}");
    assert!(body.contains("outcome conflict"), "{body}");
    assert!(body.contains("README"), "{body}");
    for c in [&c1, &c3] {
        lab.until_card(c, "accepted", |c| c["column"] == "accepted")
            .await;
    }
    let jobs = lab.until_idle().await;
    assert_eq!(jobs.len(), 1, "{jobs:#?}");
    assert_eq!(jobs[0]["state"], "passed");
    assert_eq!(jobs[0]["batch"][1]["outcome"], "conflict", "{jobs:#?}");
    git(&lab.origin, &["merge-base", "--is-ancestor", &h3, "next"]);
    assert_eq!(
        next_merges(&lab),
        ["Merge ter-k1 onto next", "Merge ter-k3 onto next"]
    );
    assert!(
        comments(&lab.card_of(&c2).await).contains("left the batch on a conflict"),
        "{k2}"
    );
    lab.settle().await;
    assert_eq!(lab.failed_rule_runs().await, Vec::<Value>::new());
}

both_flows!(a_merge_that_fails_without_a_conflict_drops_only_that_card);
/// TER-112 review F4: a branch whose merge fails without a conflict (an
/// unrelated history) leaves the batch alone; the others merge in the same
/// job.
async fn a_merge_that_fails_without_a_conflict_drops_only_that_card(flow: &'static str) {
    let lab = lab_flow(2, flow).await;
    let h1 = lab.branch("ter-u1", "next", &[("u1", "1"), ("OK", "y\n")]);
    // An orphan branch: no history in common with `next`.
    git(&lab.work, &["checkout", "-q", "--orphan", "ter-u2"]);
    git(&lab.work, &["rm", "-rq", "--cached", "."]);
    std::fs::write(lab.work.join("u2"), "2").unwrap();
    git(&lab.work, &["add", "u2"]);
    git(&lab.work, &["commit", "-q", "-m", "orphan"]);
    git(&lab.work, &["push", "-q", "-f", "origin", "ter-u2"]);
    git(&lab.work, &["clean", "-fdq"]);
    let h3 = lab.branch("ter-u3", "next", &[("u3", "3"), ("OK", "y\n")]);
    let c1 = lab.card("u1", "ter-u1").await;
    let c2 = lab.card("u2", "ter-u2").await;
    let c3 = lab.card("u3", "ter-u3").await;
    let r = lab
        .td
        .post_as(&lab.lead, "/api/merges", json!({"cards": [c1, c2, c3]}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let u2 = lab
        .until_card(&c2, "ready", |c| c["column"] == "ready")
        .await;
    let body = u2["body"].as_str().unwrap();
    assert!(body.contains("outcome conflict"), "{body}");
    assert!(body.contains("without a conflict"), "{body}");
    for c in [&c1, &c3] {
        lab.until_card(c, "accepted", |c| c["column"] == "accepted")
            .await;
    }
    let jobs = lab.until_idle().await;
    assert_eq!(jobs.len(), 1, "{jobs:#?}");
    assert_eq!(jobs[0]["state"], "passed");
    for h in [&h1, &h3] {
        git(&lab.origin, &["merge-base", "--is-ancestor", h, "next"]);
    }
    assert_eq!(
        next_merges(&lab),
        ["Merge ter-u1 onto next", "Merge ter-u3 onto next"]
    );
    lab.settle().await;
    assert_eq!(lab.failed_rule_runs().await, Vec::<Value>::new());
}

// ------------------------------------------------------------- TER-123

/// A process group that a job run of the last daemon left on this
/// machine: a shell that ignores `TERM` with a child. Its job PID file
/// holds the PID and the start time, as the wrapper writes them.
fn orphan(job: &str) -> (std::process::Child, u32) {
    use std::os::unix::process::CommandExt;
    let dir = tempfile::tempdir().unwrap();
    let child_file = dir.path().join("child");
    let leader = std::process::Command::new("sh")
        .args([
            "-c",
            "trap '' TERM; sleep 300 & echo $! > \"$1\"; wait",
            "orphan",
        ])
        .arg(&child_file)
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = leader.id();
    let start = Instant::now();
    let child = loop {
        if let Some(c) = std::fs::read_to_string(&child_file)
            .ok()
            .and_then(|s| s.trim().parse().ok())
        {
            break c;
        }
        assert!(start.elapsed() < DEADLINE, "the orphan wrote no child");
        std::thread::sleep(Duration::from_millis(20));
    };
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let st = stat
        .rsplit(") ")
        .next()
        .unwrap()
        .split_whitespace()
        .nth(19)
        .unwrap()
        .to_owned();
    std::fs::write(
        terrariumd::run_kill::job_pidfile(job),
        format!("{pid} {st}\n"),
    )
    .unwrap();
    (leader, child)
}

/// Wait until the fake docker has held every kill before execution.
async fn until_kills_held(gate: &Path, ids: &[&str]) {
    let start = Instant::now();
    for id in ids {
        let f = terrariumd::run_kill::job_pidfile(id);
        while !gate.join(Path::new(&f).file_name().unwrap()).exists() {
            assert!(start.elapsed() < DEADLINE, "the kill of {id} never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

both_flows!(lost_runs_are_killed_when_the_host_agent_is_back_and_keep_their_slots);
/// TER-123 review, blocker 1 and major 3: after a daemon restart, a job
/// that was running and one cancelled while it drained left their
/// processes on a docker target (through host agent `lab1`). The host
/// agent connects only after the start. The recovery waits for it, kills
/// both process groups, and puts the running job back in the queue with
/// its attempt (P5: a restart is not a failure). Until then both jobs
/// keep their slots: a validation queued meanwhile (two slots) does not
/// start, also not on a tick.
async fn lost_runs_are_killed_when_the_host_agent_is_back_and_keep_their_slots(flow: &'static str) {
    let (lab, bins, children) = docker_lab_flow(2, 600, flow).await;
    let gate = tempfile::Builder::new()
        .prefix("kill-gate-owner")
        .tempdir_in(bins.path())
        .unwrap();
    let kill_gate = bins.path().join("kill-gate");
    std::os::unix::fs::symlink(gate.path(), &kill_gate).unwrap();
    let state = lab.td.daemon.as_ref().unwrap().state.clone();
    // The running job runs again after its requeue: it needs its branch.
    lab.branch("ter-l1", "next", &[("OK", "1")]);
    let cr = lab.card("lost-run", "ter-l1").await;
    let cc = lab.card("lost-cancel", "ter-l2").await;
    let run_id = ulid::Ulid::new().to_string();
    let cancel_id = ulid::Ulid::new().to_string();
    let now = chrono::Utc::now();
    let at = |mins: i64| {
        (now - chrono::Duration::minutes(mins)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    };
    let rows = [
        (run_id.clone(), cr.clone(), "ter-l1", "running", at(5), None),
        (
            cancel_id.clone(),
            cc.clone(),
            "b",
            "cancelled",
            at(5),
            Some(at(1)),
        ),
    ];
    state
        .db
        .write("operator", move |t| {
            for (id, card, branch, st, started, ended) in rows {
                t.execute(
                    "INSERT INTO jobs (id, project, card, kind, repo, branch, state, attempt,
                                       created_by, created_at, started_at, ended_at)
                     VALUES (?1, 'p', ?2, 'validate', 'app', ?3, ?4, 1, 't', ?5, ?5, ?6)",
                    rusqlite::params![id, card, branch, st, started, ended],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
    let (mut lr, run_child) = orphan(&run_id);
    let (mut lc, cancel_child) = orphan(&cancel_id);

    // The restart: the host agent is not connected yet.
    lab.agent_down().await;
    let recovery = terrariumd::jobs::recover_lost(&state).await;
    let h = lab.branch("ter-l3", "next", &[("OK", "1")]);
    let c = lab.card("queued", "ter-l3").await;
    // A validation queued meanwhile. The validate API checks the target
    // and refuses while the host agent is offline, so the row goes in as
    // the API writes it, and a tick follows as after the API.
    let queued = ulid::Ulid::new().to_string();
    let (qid, qcard, qat) = (queued.clone(), c.clone(), at(0));
    state
        .db
        .write("operator", move |t| {
            t.execute(
                "INSERT INTO jobs (id, project, card, kind, repo, branch, commit_sha, state,
                                   attempt, created_by, created_at)
                 VALUES (?1, 'p', ?2, 'validate', 'app', 'ter-l3', ?3, 'queued', 1, 't', ?4)",
                rusqlite::params![qid, qcard, h, qat],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    terrariumd::jobs::tick(&state).await.unwrap();
    for (p, what) in [
        (lr.id(), "running"),
        (run_child, "running child"),
        (lc.id(), "cancelled"),
        (cancel_child, "cancelled child"),
    ] {
        assert!(
            alive(p),
            "the {what} orphan {p} is gone before the host agent is back"
        );
    }
    let q = lab
        .td
        .get(&format!("/api/jobs/{queued}"))
        .await
        .ok()
        .clone();
    assert_eq!(q["state"], "queued", "a job took a slot of a lost job: {q}");
    let j = lab
        .td
        .get(&format!("/api/jobs/{run_id}"))
        .await
        .ok()
        .clone();
    assert_eq!(j["state"], "running", "{j}");

    // TER-160 holds a queued job while its host agent is offline, so the
    // check above alone does not prove the slots are held (review 2,
    // minor 3). Check again with the host agent back, while the fake
    // docker holds both kills. No scheduling delay can finish them early.
    lab.agent_up().await;
    until_kills_held(&kill_gate, &[&run_id, &cancel_id]).await;
    terrariumd::jobs::tick(&state).await.unwrap();
    let q = lab
        .td
        .get(&format!("/api/jobs/{queued}"))
        .await
        .ok()
        .clone();
    assert_eq!(
        q["state"], "queued",
        "a job took a slot of a lost job while its kill ran: {q}"
    );
    for p in [lr.id(), lc.id()] {
        assert!(alive(p), "the kill of {p} ended before the check");
    }
    drop(gate);
    tokio::time::timeout(DEADLINE, recovery)
        .await
        .unwrap()
        .unwrap();
    for p in [lr.id(), run_child, lc.id(), cancel_child] {
        assert!(!alive(p), "the orphan {p} outlived the recovery");
    }
    let j = lab
        .td
        .get(&format!("/api/jobs/{run_id}"))
        .await
        .ok()
        .clone();
    // P5: requeued, not failed. It may run again already: its tail stays
    // until it ends, and its recipe runs for minutes.
    assert!(
        j["state"] == "queued" || j["state"] == "running",
        "a lost job is not back in the queue: {j}"
    );
    assert_eq!(j["attempt"], 1, "a restart used an attempt: {j}");
    assert!(
        j["tail"]
            .as_str()
            .unwrap_or("")
            .contains("waits in the queue again"),
        "{j}"
    );
    let card = lab.td.get(&format!("/api/cards/{cr}")).await.ok().clone();
    assert_ne!(card["column"], "ready", "a restart moved the card: {card}");
    // The coded flow labels validation requeues. The default rules pack
    // labels only merge lifecycle events (TER-218).
    lab.settle().await;
    let card = lab.card_of(&cr).await;
    let l: Vec<String> = labels(&card)
        .into_iter()
        .filter(|l| l.starts_with("validation:"))
        .collect();
    assert_eq!(l.len(), usize::from(flow == "code"), "{card}");
    // The requeued job writes its own file again when it runs.
    let f = terrariumd::run_kill::job_pidfile(&cancel_id);
    assert!(!Path::new(&f).exists(), "{f} is left");
    // Now the slots are free: the requeued job and the queued validation
    // start. End them.
    until_not(&lab, &run_id, "queued").await;
    until_not(&lab, &queued, "queued").await;
    for id in [&run_id, &queued] {
        let r = lab.td.delete(&format!("/api/jobs/{id}")).await;
        assert_eq!(r.ok()["state"], "cancelled");
    }
    if let Ok(text) = std::fs::read_to_string(&children) {
        for p in text.lines().filter_map(|l| l.trim().parse::<u32>().ok()) {
            let start = Instant::now();
            while alive(p) {
                assert!(start.elapsed() < DEADLINE, "the child {p} outlived its job");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    let _ = lr.wait();
    let _ = lc.wait();
}

both_flows!(a_lost_job_cancelled_during_its_kill_keeps_its_slot);
/// TER-123 review 2, major 1: a lost job that is still running gets a
/// cancel through the API while its kill waits for the host agent. Its
/// slot stays taken until the kill ended (one slot): a queued validation
/// does not start while the kill runs, and starts after it.
async fn a_lost_job_cancelled_during_its_kill_keeps_its_slot(flow: &'static str) {
    let (lab, bins, children) = docker_lab_flow(1, 600, flow).await;
    let gate = tempfile::Builder::new()
        .prefix("kill-gate-owner")
        .tempdir_in(bins.path())
        .unwrap();
    let kill_gate = bins.path().join("kill-gate");
    std::os::unix::fs::symlink(gate.path(), &kill_gate).unwrap();
    let state = lab.td.daemon.as_ref().unwrap().state.clone();
    let cr = lab.card("lost-run", "ter-m1").await;
    let run_id = ulid::Ulid::new().to_string();
    let at = |mins: i64| {
        (chrono::Utc::now() - chrono::Duration::minutes(mins))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    };
    let (rid, rcard, rat) = (run_id.clone(), cr.clone(), at(5));
    state
        .db
        .write("operator", move |t| {
            t.execute(
                "INSERT INTO jobs (id, project, card, kind, repo, branch, state, created_by,
                                   created_at, started_at)
                 VALUES (?1, 'p', ?2, 'validate', 'app', 'b', 'running', 't', ?3, ?3)",
                rusqlite::params![rid, rcard, rat],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let (mut lr, run_child) = orphan(&run_id);

    // The restart: the host agent is not connected yet.
    lab.agent_down().await;
    let recovery = terrariumd::jobs::recover_lost(&state).await;
    // The operator cancels the lost job that looks stuck.
    let r = lab.td.delete(&format!("/api/jobs/{run_id}")).await;
    assert_eq!(r.ok()["state"], "cancelled");
    let h = lab.branch("ter-m2", "next", &[("OK", "1")]);
    let c = lab.card("queued", "ter-m2").await;
    let queued = ulid::Ulid::new().to_string();
    let (qid, qcard, qat) = (queued.clone(), c.clone(), at(0));
    state
        .db
        .write("operator", move |t| {
            t.execute(
                "INSERT INTO jobs (id, project, card, kind, repo, branch, commit_sha, state,
                                   attempt, created_by, created_at)
                 VALUES (?1, 'p', ?2, 'validate', 'app', 'ter-m2', ?3, 'queued', 1, 't', ?4)",
                rusqlite::params![qid, qcard, h, qat],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    // The host agent is back and the kill runs: the slot is still taken.
    lab.agent_up().await;
    until_kills_held(&kill_gate, &[&run_id]).await;
    terrariumd::jobs::tick(&state).await.unwrap();
    let q = lab
        .td
        .get(&format!("/api/jobs/{queued}"))
        .await
        .ok()
        .clone();
    assert_eq!(
        q["state"], "queued",
        "a job took the slot of a cancelled lost job while its kill ran: {q}"
    );
    assert!(alive(lr.id()), "the kill ended before the check");

    drop(gate);
    tokio::time::timeout(DEADLINE, recovery)
        .await
        .unwrap()
        .unwrap();
    for p in [lr.id(), run_child] {
        assert!(!alive(p), "the orphan {p} outlived the recovery");
    }
    let j = lab
        .td
        .get(&format!("/api/jobs/{run_id}"))
        .await
        .ok()
        .clone();
    assert_eq!(j["state"], "cancelled", "{j}");
    // Now the slot is free: the queued validation starts. End it.
    until_not(&lab, &queued, "queued").await;
    let r = lab.td.delete(&format!("/api/jobs/{queued}")).await;
    assert_eq!(r.ok()["state"], "cancelled");
    if let Ok(text) = std::fs::read_to_string(&children) {
        for p in text.lines().filter_map(|l| l.trim().parse::<u32>().ok()) {
            let start = Instant::now();
            while alive(p) {
                assert!(start.elapsed() < DEADLINE, "the child {p} outlived its job");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    let _ = lr.wait();
}

both_flows!(a_lost_job_without_a_confirmed_kill_gets_its_slot_wiped);
/// TER-190 review, finding 2: a lost running job ran on another host than
/// the delivery target's now, so its kill cannot confirm that its
/// processes ended. Its slot is marked, and the next job of the slot (the
/// requeued job itself) gets a new tree: the old tree goes, `target/`
/// too. The mark then goes, and the job after it keeps `target/` again.
async fn a_lost_job_without_a_confirmed_kill_gets_its_slot_wiped(flow: &'static str) {
    use terrariumd::store::jobs;
    let lab = lab_flow(1, flow).await;
    let state = lab.td.daemon.as_ref().unwrap().state.clone();
    let wd = lab._dirs.path().join("jobs");
    let slot = wd.join("p/slot-1");
    std::fs::create_dir_all(slot.join("target")).unwrap();
    std::fs::write(slot.join("target/stale"), "of the lost job").unwrap();
    lab.branch("ter-w1", "next", &[("OK", "1")]);
    let card = lab.card("lost-run", "ter-w1").await;
    let run_id = ulid::Ulid::new().to_string();
    let at = (chrono::Utc::now() - chrono::Duration::minutes(5))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let (rid, rcard) = (run_id.clone(), card.clone());
    state
        .db
        .write("operator", move |t| {
            t.execute(
                "INSERT INTO jobs (id, project, card, kind, repo, branch, state, attempt,
                                   created_by, created_at, started_at, host, slot)
                 VALUES (?1, 'p', ?2, 'validate', 'app', 'ter-w1', 'running', 1, 't', ?3, ?3,
                         'elsewhere', 1)",
                rusqlite::params![rid, rcard, at],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let recovery = terrariumd::jobs::recover_lost(&state).await;
    tokio::time::timeout(DEADLINE, recovery)
        .await
        .unwrap()
        .unwrap();
    let jobs = lab.until_idle().await;
    let j = jobs.iter().find(|j| j["id"] == run_id.as_str()).unwrap();
    assert_eq!(j["state"], "passed", "{j:#}");
    assert_eq!(j["slot"], 1, "{j:#}");
    assert!(
        !slot.join("target/stale").exists(),
        "the slot was not wiped"
    );
    let pending = state
        .db
        .read(|c| jobs::slot_wipe_pending(c, "p", 1))
        .await
        .unwrap();
    assert!(!pending, "the wipe mark is left");

    // The next job of the slot reuses its tree as before.
    std::fs::create_dir_all(slot.join("target")).unwrap();
    std::fs::write(slot.join("target/kept"), "built").unwrap();
    let head = lab.branch("ter-w2", "next", &[("OK", "2")]);
    let id = lab.card("next", "ter-w2").await;
    let r = lab
        .td
        .post_as(
            &lab.w1,
            &format!("/api/cards/{id}/validate"),
            json!({"commit": head}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let jobs = lab.until_idle().await;
    assert!(jobs.iter().all(|j| j["state"] == "passed"), "{jobs:#?}");
    assert!(slot.join("target/kept").exists(), "a second wipe ran");
}

/// Set the delivery of `p`: `target`, one slot, a recipe that sleeps on
/// a branch with `SLOW`.
async fn deliver_to(lab: &Lab, target: &str) {
    let work_dir = lab._dirs.path().join(format!("jobs-{target}"));
    let r = lab
        .td
        .put(
            "/api/projects/p/delivery",
            json!({"target": target, "recipe": "if test -f SLOW; then sleep 8; fi; test -f OK",
                   "slots": 1, "next_repos": ["app"],
                   "work_dir": work_dir.display().to_string()}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
}

/// Queue a validation of a branch with `OK` (and `SLOW` when `slow`) on
/// card `title`, as `w1`. Returns the card.
async fn queue_green(lab: &Lab, branch: &str, title: &str, slow: bool) -> String {
    let mut files = vec![("OK", "yes\n")];
    if slow {
        files.push(("SLOW", "yes\n"));
    }
    let head = lab.branch(branch, "next", &files);
    let id = lab.card(title, branch).await;
    let r = lab
        .td
        .post_as(
            &lab.w1,
            &format!("/api/cards/{id}/validate"),
            json!({"commit": head}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    id
}

/// Queue a job for a card while the target `p-box` is up, behind a slow
/// job in the one slot, then point the delivery at `target`: the card's
/// job is picked for `target` when the slow job ends. The validate call
/// reads the branch head on the target, so a job is only queued while its
/// target is up (as in TER-160: queued before the outage). Returns the
/// card.
async fn queue_for(lab: &Lab, target: &str) -> String {
    deliver_to(lab, "p-box").await;
    let blocker = queue_green(lab, "ter-slow", "slow blocker", true).await;
    let start = Instant::now();
    while !lab.jobs().await.iter().any(|j| j["state"] == "running") {
        assert!(start.elapsed() < DEADLINE, "the blocker never started");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let id = queue_green(lab, "ter-wait", "waits for its host", false).await;
    deliver_to(lab, target).await;
    lab.until_card(&blocker, "review", |c| c["column"] == "review")
        .await;
    id
}

/// The job of card `id`.
async fn job_of(lab: &Lab, id: &str) -> Vec<Value> {
    lab.jobs()
        .await
        .into_iter()
        .filter(|j| j["card"] == id)
        .collect()
}

/// TER-123, P5: a deploy drains the daemon first. During the drain a
/// queued job does not start (a free slot, a tick too), the running job
/// goes on and is listed in `drain.jobs`, and the drain waits for it.
/// The end of the drain starts the queued job.
#[tokio::test]
async fn a_drain_starts_no_job_and_lists_the_running_ones() {
    let lab = lab(2).await;
    let state = lab.td.daemon.as_ref().unwrap().state.clone();
    let work_dir = lab._dirs.path().join("jobs-drain");
    let r = lab
        .td
        .put(
            "/api/projects/p/delivery",
            json!({"target": "p-box", "recipe": "if test -f SLOW; then sleep 6; fi; test -f OK",
                   "slots": 2, "next_repos": ["app"],
                   "work_dir": work_dir.display().to_string()}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let slow = queue_green(&lab, "ter-d1", "runs into the drain", true).await;
    let start = Instant::now();
    let running = loop {
        let j = job_of(&lab, &slow).await;
        if j.first().is_some_and(|j| j["state"] == "running") {
            break j[0]["id"].as_str().unwrap().to_owned();
        }
        assert!(
            start.elapsed() < DEADLINE,
            "the slow job never started: {j:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let r = lab.td.post_empty("/api/system/update/drain").await;
    let d = r.ok().clone();
    assert_eq!(d["on"], true, "{d}");
    assert_eq!(d["jobs"], json!([running]), "{d}");

    // A free slot, a queued job, a tick: it does not start.
    let fast = queue_green(&lab, "ter-d2", "queued in the drain", false).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    terrariumd::jobs::tick(&state).await.unwrap();
    let j = job_of(&lab, &fast).await;
    assert_eq!(
        j[0]["state"], "queued",
        "a job started during the drain: {j:?}"
    );
    assert_eq!(j[0]["attempt"], 1, "{j:?}");

    // The running job goes on to its end; then the drain lists no job.
    lab.until_card(&slow, "review", |c| c["column"] == "review")
        .await;
    let v = lab.td.get("/api/system/update").await.ok().clone();
    assert_eq!(v["drain"]["jobs"], json!([]), "{v}");
    let j = job_of(&lab, &fast).await;
    assert_eq!(
        j[0]["state"], "queued",
        "a job started during the drain: {j:?}"
    );

    // The end of the drain starts it.
    let r = lab.td.post_empty("/api/system/update/resume").await;
    assert_eq!(r.ok()["on"], false);
    lab.until_card(&fast, "review", |c| c["column"] == "review")
        .await;
}

both_flows!(a_job_waits_for_an_offline_host_agent_and_starts_when_it_connects);
/// TER-160: the host agent of the target is offline when the queue picks
/// the job. The job stays queued with no attempt used and the card does
/// not move; the job starts once the host agent connects, and passes.
async fn a_job_waits_for_an_offline_host_agent_and_starts_when_it_connects(flow: &'static str) {
    let lab = lab_flow(2, flow).await;
    let _ticks = QueueTicks::start(&lab);
    let td = &lab.td;
    let r = td.post("/api/hosts", json!({"name": "lab2"})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let token = r.body["token"].as_str().unwrap().to_owned();
    let r = td
        .post(
            "/api/targets",
            json!({"name": "p-off", "kind": "host", "host": "lab2", "project": "p"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let id = queue_for(&lab, "p-off").await;
    // Exercise repeated queue picks without waiting for its background timer.
    for _ in 0..3 {
        terrariumd::jobs::tick(&td.daemon.as_ref().unwrap().state)
            .await
            .unwrap();
    }
    let jobs = job_of(&lab, &id).await;
    assert_eq!(jobs.len(), 1, "no rerun, no second job: {jobs:?}");
    assert_eq!(jobs[0]["state"], "queued", "{jobs:?}");
    assert_eq!(jobs[0]["attempt"], 1, "{jobs:?}");
    assert!(jobs[0]["started_at"].is_null(), "{jobs:?}");
    lab.settle().await;
    let c = lab.card_of(&id).await;
    assert_eq!(c["column"], "validating", "{c}");
    assert_eq!(c["assignee"], lab.w1.as_str(), "{c}");
    assert!(labels(&c).contains(&"validation:queued".to_owned()), "{c}");
    assert!(
        !c["body"].as_str().unwrap().contains("Validation failed"),
        "{c}"
    );
    // The host agent connects: the job starts and passes.
    connect_host(
        td,
        lab.hosts_addr,
        "lab2",
        &token,
        &lab._dirs.path().join("lab2"),
        None,
    )
    .await;
    lab.until_card(&id, "review", |c| c["column"] == "review")
        .await;
    lab.until_idle().await;
    let jobs = job_of(&lab, &id).await;
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_eq!(jobs[0]["state"], "passed", "{jobs:?}");
    assert_eq!(jobs[0]["attempt"], 1, "{jobs:?}");
}

both_flows!(a_refused_ssh_connect_puts_the_job_back_in_the_queue);
/// TER-160: an ssh target that refuses the connection before the recipe
/// starts. The job goes back to the queue with no attempt used, the tail
/// says why it waits, and the card does not move.
async fn a_refused_ssh_connect_puts_the_job_back_in_the_queue(flow: &'static str) {
    let lab = lab_flow(2, flow).await;
    let td = &lab.td;
    // Port 1 on the loopback: nothing listens, the connect is refused.
    let r = td
        .post(
            "/api/targets",
            json!({"name": "p-ssh", "kind": "ssh", "address": "ci@127.0.0.1", "port": 1,
                   "project": "p"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let id = queue_for(&lab, "p-ssh").await;
    let start = Instant::now();
    let job = loop {
        let jobs = job_of(&lab, &id).await;
        assert_eq!(jobs.len(), 1, "no rerun, no second job: {jobs:?}");
        let j = jobs[0].clone();
        assert_ne!(j["state"], "failed", "{j}");
        let jid = j["id"].as_str().unwrap();
        let full = td.get(&format!("/api/jobs/{jid}")).await.ok().clone();
        if full["state"] == "queued"
            && full["tail"]
                .as_str()
                .is_some_and(|t| t.contains("waits for its host"))
        {
            break full;
        }
        assert!(
            start.elapsed() < DEADLINE,
            "the job never went back: {full}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let tail = job["tail"].as_str().unwrap();
    assert!(tail.contains("Connection refused"), "{tail}");
    assert_eq!(job["attempt"], 1, "{job}");
    assert!(job["started_at"].is_null(), "{job}");
    lab.settle().await;
    let c = lab.card_of(&id).await;
    assert_eq!(c["column"], "validating", "{c}");
    assert_eq!(c["assignee"], lab.w1.as_str(), "{c}");
    let l = labels(&c);
    assert!(l.contains(&"validation:queued".to_owned()), "{l:?}");
    assert!(!l.contains(&"validation:running".to_owned()), "{l:?}");
    // It waits for its next try: still queued and no attempt used after a
    // tick of the queue.
    tokio::time::sleep(Duration::from_secs(6)).await;
    let jobs = job_of(&lab, &id).await;
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_eq!(jobs[0]["state"], "queued", "{jobs:?}");
    assert_eq!(jobs[0]["attempt"], 1, "{jobs:?}");
}

/// Validate the card on `branch` with the current recipe and wait for
/// the end of its job.
async fn validate_card(lab: &Lab, branch: &str, head: &str) -> Value {
    let id = lab.card(branch, branch).await;
    let r = lab
        .td
        .post_as(
            &lab.w1,
            &format!("/api/cards/{id}/validate"),
            json!({"commit": head}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let jobs = lab.until_idle().await;
    jobs.into_iter().find(|j| j["card"] == id.as_str()).unwrap()
}

/// TER-100: the recipe gets the job kind, the head, the base, and the
/// file of the changed paths. A validate job diffs against the merge-base
/// with `next` (a later change on `next` is not in the list), a merge job
/// against `next` before the merge. Without `next`, no base and no list.
#[tokio::test]
async fn the_recipe_gets_the_job_kind_and_the_changed_files() {
    let lab = lab(2).await;
    let out = lab._dirs.path().join("env");
    std::fs::create_dir_all(&out).unwrap();
    let recipe = format!(
        "test -f OK && f='{}/'\"$TERRARIUM_JOB_KIND-$TERRARIUM_JOB_ID\" \
         && printf 'kind=%s\\nhead=%s\\nbase=%s\\nfiles=%s\\n' \"$TERRARIUM_JOB_KIND\" \
            \"$TERRARIUM_HEAD\" \"${{TERRARIUM_BASE-unset}}\" \"${{TERRARIUM_CHANGED_FILES-unset}}\" > \"$f\" \
         && if [ -n \"${{TERRARIUM_CHANGED_FILES:-}}\" ]; then cat \"$TERRARIUM_CHANGED_FILES\" >> \"$f\"; fi",
        out.display()
    );
    set_recipe(&lab, &recipe).await;
    let fork = git(&lab.origin, &["rev-parse", "next"]);
    let head = lab.branch("ter-env", "next", &[("OK", "yes\n"), ("a.rs", "a\n")]);
    // `next` moves after the fork: its change is not the card's.
    lab.branch("ter-other", "next", &[("other", "x\n")]);
    git(&lab.work, &["push", "-q", "origin", "ter-other:next"]);
    let moved = git(&lab.origin, &["rev-parse", "next"]);
    let read = |job: &Value| {
        let name = format!(
            "{}-{}",
            job["kind"].as_str().unwrap(),
            job["id"].as_str().unwrap()
        );
        std::fs::read_to_string(out.join(name)).unwrap()
    };

    let job = validate_card(&lab, "ter-env", &head).await;
    assert_eq!(job["state"], "passed", "{job:#}");
    let text = read(&job);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "kind=validate", "{text}");
    assert_eq!(lines[1], format!("head={head}"), "{text}");
    assert_eq!(lines[2], format!("base={fork}"), "{text}");
    assert!(
        lines[3].starts_with("files=/") && lines[3].ends_with("/.git/terrarium/changed-files"),
        "{text}"
    );
    assert_eq!(lines[4..], ["OK", "a.rs"], "{text}");

    let job = merge_with_recipe(&lab, "ter-env2", &recipe).await;
    assert_eq!(job["state"], "passed", "{job:#}");
    let text = read(&job);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "kind=merge", "{text}");
    assert_eq!(lines[2], format!("base={moved}"), "{text}");
    assert_eq!(lines[4..], ["OK"], "{text}");

    // No `next`: the job runs, without a base and a list.
    git(&lab.work, &["push", "-q", "origin", ":next"]);
    let job = validate_card(&lab, "ter-env", &head).await;
    assert_eq!(job["state"], "passed", "{job:#}");
    let text = read(&job);
    assert!(text.contains("base=unset\nfiles=unset\n"), "{text}");
    assert_eq!(text.lines().count(), 4, "{text}");
}

// ------------------------------------------------------------- TER-196

/// TER-196: the host agent restarts while the recipe of a batch merge
/// runs, so the daemon gets "the host agent no longer holds the session".
/// That is no red: the batch does not split, the same job goes back to
/// the queue with its attempt, reruns whole, and both cards merge.
#[tokio::test]
async fn a_lost_host_agent_session_reruns_the_whole_batch() {
    let lab = lab(2).await;
    let marks = tempfile::tempdir().unwrap();
    let m = marks.path().display().to_string();
    // Each run counts itself. The first run waits to be lost, far longer
    // than the test: only the kill of the lost run ends it. The rerun
    // passes.
    set_recipe(
        &lab,
        &format!(
            "test -f OK && test -f s1 && test -f s2 && echo run >> '{m}/runs' && \
             if test -f '{m}/ran'; then exit 0; fi; echo $$ > '{m}/pid'; touch '{m}/ran'; sleep 600"
        ),
    )
    .await;
    lab.branch("ter-s1", "next", &[("OK", "yes\n"), ("s1", "1")]);
    lab.branch("ter-s2", "next", &[("OK", "yes\n"), ("s2", "2")]);
    let c1 = lab.card("s1", "ter-s1").await;
    let c2 = lab.card("s2", "ter-s2").await;
    let r = lab
        .td
        .post_as(&lab.lead, "/api/merges", json!({"cards": [c1, c2]}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let job_id = r.body["job"]["id"].as_str().unwrap().to_owned();

    // The recipe runs: restart the host agent. The new one does not hold
    // the session of the run.
    let ran = marks.path().join("ran");
    let start = Instant::now();
    while !ran.exists() {
        assert!(start.elapsed() < DEADLINE, "the recipe never started");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let pid: u32 = std::fs::read_to_string(marks.path().join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    lab.agent_down().await;
    lab.agent_up().await;

    for c in [&c1, &c2] {
        let card = lab
            .until_card(c, "accepted", |c| c["column"] == "accepted")
            .await;
        let text = comments(&card);
        assert!(!text.contains("The batch splits"), "{card}");
        assert!(
            !card["body"].as_str().unwrap().contains("Validation failed"),
            "{card}"
        );
    }
    // The lost run's processes got the kill of a lost run: its recipe
    // sleeps 600 s, so nothing else ends it by now (review, major 1).
    if alive(pid) {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .status();
        panic!("the lost run {pid} outlived its rerun");
    }
    let jobs = lab.until_idle().await;
    assert_eq!(jobs.len(), 1, "the batch did not split: {jobs:#?}");
    let j = lab
        .td
        .get(&format!("/api/jobs/{job_id}"))
        .await
        .ok()
        .clone();
    assert_eq!(j["state"], "passed", "{j:#}");
    assert_eq!(j["attempt"], 1, "a lost session used an attempt: {j:#}");
    assert_eq!(j["batch"].as_array().unwrap().len(), 2, "{j:#}");
    let runs = std::fs::read_to_string(marks.path().join("runs")).unwrap();
    assert_eq!(runs.lines().count(), 2, "the same job ran twice: {runs}");
    assert_eq!(
        next_merges(&lab),
        ["Merge ter-s1 onto next", "Merge ter-s2 onto next"]
    );
}

/// TER-196 review 2, major 1: a run that the infrastructure lost past
/// `MAX_LOST_RERUNS` ends red, but only after the kill of a lost run. The
/// delivery moved to another host while the recipe ran, so that kill
/// cannot confirm the end, and the slot gets its wipe mark: only the kill
/// of a lost run writes it.
#[tokio::test]
async fn a_lost_run_past_the_cap_ends_red_after_the_kill() {
    let lab = lab(1).await;
    let td = &lab.td;
    let state = td.daemon.as_ref().unwrap().state.clone();
    let marks = tempfile::tempdir().unwrap();
    let m = marks.path().display().to_string();
    let work_dir = lab._dirs.path().join("jobs").display().to_string();
    let recipe = format!("echo $$ > '{m}/pid'; touch '{m}/ran'; sleep 600");
    let deliver = |target: &str| {
        json!({"target": target, "recipe": recipe, "slots": 1, "next_repos": ["app"],
               "work_dir": work_dir})
    };
    let r = td.put("/api/projects/p/delivery", deliver("p-box")).await;
    assert_eq!(r.status, 200, "{}", r.body);
    // A second host agent, for the move of the delivery.
    let r = td.post("/api/hosts", json!({"name": "lab2"})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let token = r.body["token"].as_str().unwrap().to_owned();
    let r = td
        .post(
            "/api/targets",
            json!({"name": "p-two", "kind": "host", "host": "lab2", "project": "p"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    connect_host(
        td,
        lab.hosts_addr,
        "lab2",
        &token,
        &lab._dirs.path().join("lab2"),
        None,
    )
    .await;
    let head = lab.branch("ter-cap", "next", &[("OK", "yes\n")]);
    let card = lab.card("capped", "ter-cap").await;
    let r = td
        .post_as(
            &lab.w1,
            &format!("/api/cards/{card}/validate"),
            json!({"commit": head}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let ran = marks.path().join("ran");
    let start = Instant::now();
    while !ran.exists() {
        assert!(start.elapsed() < DEADLINE, "the recipe never started");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let job_id = job_of(&lab, &card).await[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let pid: u32 = std::fs::read_to_string(marks.path().join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // Its last attempt, and its lost runs at the cap: the next lost run
    // is red, and nothing runs after it.
    let jid = job_id.clone();
    state
        .db
        .write("operator", move |t| {
            t.execute(
                "UPDATE jobs SET attempt = 2 WHERE id = ?1",
                rusqlite::params![jid],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    terrariumd::jobs::set_lost_runs(&job_id, terrariumd::jobs::MAX_LOST_RERUNS);
    let r = td.put("/api/projects/p/delivery", deliver("p-two")).await;
    assert_eq!(r.status, 200, "{}", r.body);
    lab.agent_down().await;
    lab.agent_up().await;

    let start = Instant::now();
    let j = loop {
        let j = td.get(&format!("/api/jobs/{job_id}")).await.ok().clone();
        if j["state"] != "running" && j["state"] != "queued" {
            break j;
        }
        assert!(start.elapsed() < DEADLINE, "the job never ended: {j:#}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(j["state"], "failed", "{j:#}");
    assert_eq!(j["outcome"], "red", "{j:#}");
    let tail = j["tail"].as_str().unwrap();
    let cap = format!(
        "the infrastructure lost the run {} times in a row",
        terrariumd::jobs::MAX_LOST_RERUNS + 1
    );
    assert!(tail.starts_with(&cap), "{tail}");
    let pending = state
        .db
        .read(|c| terrariumd::store::jobs::slot_wipe_pending(c, "p", 1))
        .await
        .unwrap();
    assert!(pending, "the capped lost run got no kill of a lost run");
    lab.until_card(&card, "ready", |c| c["column"] == "ready")
        .await;
    if alive(pid) {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .status();
        panic!("the capped lost run {pid} outlived its end");
    }
}
