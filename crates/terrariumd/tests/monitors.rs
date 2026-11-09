//! Monitors and the project dashboard (docs/api.md §30): the rights per
//! role, the API, the runs on a real host agent (`terrarium_host::Client`
//! on this machine), the status events, one run at a time, the timeout,
//! a target that is down, and the prune. The tick tests run at times
//! hours ahead of the real clock, so the daemon's own timer never runs
//! these monitors.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::time::Duration;

use chrono::{DateTime, Utc};
use common::TestDaemon;
use serde_json::{Value, json};
use terrarium_core::protocol::HostCapabilities;
use terrarium_host::{Client, ClientOptions};
use terrariumd::api::Shared;

struct Lab {
    td: TestDaemon,
    addr: std::net::SocketAddr,
}

impl Lab {
    async fn start() -> Self {
        let td = TestDaemon::start().await;
        let state = td.daemon.as_ref().unwrap().state.clone();
        let (_task, addr) = terrariumd::hosts::serve(state, "127.0.0.1:0")
            .await
            .unwrap();
        Self { td, addr }
    }

    fn state(&self) -> Shared {
        self.td.daemon.as_ref().unwrap().state.clone()
    }

    async fn register(&self, name: &str) -> String {
        let r = self.td.post("/api/hosts", json!({"name": name})).await;
        assert_eq!(r.status, 201, "{}", r.body);
        r.body["token"].as_str().unwrap().to_owned()
    }

    /// Register `name`, run its host agent, and wait for it to be online.
    async fn host(&self, name: &str) -> tokio::task::JoinHandle<()> {
        let token = self.register(name).await;
        let mut o = ClientOptions::new(format!("ws://{}/hosts/connect", self.addr), &token, name);
        o.capabilities = HostCapabilities {
            shell: Some("/bin/sh".into()),
            ..Default::default()
        };
        o.backoff_min = Duration::from_millis(50);
        o.backoff_max = Duration::from_millis(200);
        o.paths = terrarium_host::HostPaths {
            work_dir: self.td.state.with_file_name("work"),
            instances_dir: self.td.state.with_file_name("instances"),
            repos_dir: self.td.state.with_file_name("repos"),
        };
        let task = tokio::spawn(Client::new(o).run());
        for _ in 0..300 {
            if self.td.get(&format!("/api/hosts/{name}")).await.body["online"] == true {
                return task;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("host {name} never came online");
    }

    async fn target(&self, def: Value) {
        let r = self.td.post("/api/targets", def).await;
        assert_eq!(r.status, 201, "{}", r.body);
    }
}

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

fn hours(n: i64) -> DateTime<Utc> {
    Utc::now() + chrono::Duration::hours(n)
}

/// A disabled monitor body: the daemon's own timer does not run it.
fn body(name: &str, target: &str, script: &str, kind: &str) -> Value {
    json!({"name": name, "target": target, "script": script, "kind": kind,
           "schedule": {"kind": "interval", "every": "30s"}, "enabled": false})
}

async fn delete_as(td: &TestDaemon, agent: &str, path: &str) -> common::Resp {
    td.req(hyper::Method::DELETE, path, None, Some(agent)).await
}

async fn events(td: &TestDaemon, kind: &str) -> Vec<Value> {
    td.get(&format!("/api/events?kind={kind}"))
        .await
        .ok()
        .as_array()
        .unwrap()
        .clone()
}

/// Project `terra` with a lead (project_admin, `box` in its policy), a
/// worker, and project `ops` with an agent. Target `box` on host `lab1`
/// (online), `far` on `lab9` (registered, never online), and `opsbox` of
/// project `ops`.
async fn setup(lab: &Lab) -> (String, String, String, tokio::task::JoinHandle<()>) {
    let h = lab.host("lab1").await;
    lab.register("lab9").await;
    lab.td.project("terra").await;
    lab.td.project("ops").await;
    lab.target(json!({"name": "box", "kind": "host", "host": "lab1"}))
        .await;
    lab.target(json!({"name": "far", "kind": "host", "host": "lab9"}))
        .await;
    lab.target(json!({"name": "opsbox", "kind": "host", "host": "lab1", "project": "ops"}))
        .await;
    let lead = lab
        .td
        .agent(
            "terra",
            "lead",
            json!({"roles": ["project_admin"], "policy": {"targets": ["box", "opsbox"]}}),
        )
        .await;
    let worker = lab
        .td
        .agent("terra", "worker", json!({"policy": {"targets": ["box"]}}))
        .await;
    let other = lab.td.agent("ops", "dev", json!({})).await;
    (id(&lead), id(&worker), id(&other), h)
}

#[tokio::test]
async fn rights_crud_validation_and_audit() {
    let lab = Lab::start().await;
    let (lead, worker, other, _h) = setup(&lab).await;
    let td = &lab.td;
    let path = "/api/projects/terra/monitors";

    // A worker reads, but does not create.
    td.post_as(&worker, path, body("load", "box", "echo 1", "number"))
        .await
        .err(403, "forbidden");
    // The lead creates on a target in its policy.
    let r = td
        .post_as(&lead, path, body("load", "box", "echo 1", "number"))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let load = id(&r.body);
    assert_eq!(r.body["run_as"], format!("agent:{lead}"));
    assert_eq!(r.body["timeout_secs"], 30);
    assert_eq!(r.body["retention_days"], 7);
    assert_eq!(r.body["period_secs"], 30);
    assert!(r.body["next_run_at"].is_null());
    assert!(r.body["last"].is_null());
    // Not in its policy.
    let r = td
        .post_as(&lead, path, body("far", "far", "echo 1", "number"))
        .await;
    r.err(403, "forbidden");
    // A target of another project.
    let r = td
        .post(path, body("ops", "opsbox", "echo 1", "number"))
        .await;
    r.err(422, "invalid");
    assert_eq!(r.body["error"]["details"]["field"], "target");
    // An unknown target names the field.
    let r = td
        .post(path, body("nope", "nope", "echo 1", "number"))
        .await;
    r.err(422, "invalid");
    assert_eq!(r.body["error"]["details"]["field"], "target");
    // Validation.
    let mut b = body("fast", "box", "echo 1", "number");
    b["schedule"] = json!({"kind": "interval", "every": "10s"});
    let r = td.post(path, b).await;
    r.err(422, "invalid");
    assert_eq!(r.body["error"]["details"]["field"], "schedule.every");
    let mut b = body("t", "box", "echo 1", "number");
    b["timeout_secs"] = json!(601);
    td.post(path, b).await.err(422, "invalid");
    // The same name twice.
    td.post(path, body("load", "box", "echo 2", "number"))
        .await
        .err(409, "conflict");
    // The operator creates any monitor.
    let r = td.post(path, body("up", "far", "uptime", "text")).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let up = id(&r.body);

    // Members and their agents read; another project's agent does not.
    let list = td.get_as(&worker, path).await;
    let names: Vec<&str> = list
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["load", "up"]);
    td.get_as(&other, path).await.err(403, "forbidden");
    td.get_as(&worker, "/api/monitors/terra%2Fload").await.ok();
    td.get_as(&other, &format!("/api/monitors/{load}"))
        .await
        .err(403, "forbidden");
    td.get_as(&worker, &format!("/api/monitors/{load}/results"))
        .await
        .ok();

    // Edit: a worker cannot; the lead can. A patch merges; null clears.
    td.patch_as(
        &worker,
        &format!("/api/monitors/{load}"),
        json!({"unit": "x"}),
    )
    .await
    .err(403, "forbidden");
    let r = td
        .patch_as(
            &lead,
            &format!("/api/monitors/{load}"),
            json!({"unit": "%", "warn": 80, "crit": 90, "description": "load"}),
        )
        .await;
    assert_eq!(r.ok()["unit"], "%");
    assert_eq!(r.body["crit"], 90.0);
    let r = td
        .patch(&format!("/api/monitors/{load}"), json!({"warn": null}))
        .await;
    assert!(r.ok()["warn"].is_null());
    assert_eq!(r.body["crit"], 90.0);
    // The lead cannot move the operator's monitor to a target it cannot use.
    td.patch_as(
        &lead,
        &format!("/api/monitors/{up}"),
        json!({"script": "date"}),
    )
    .await
    .err(403, "forbidden");
    // Bad fields.
    td.patch(&format!("/api/monitors/{load}"), json!({"colour": 1}))
        .await
        .err(422, "invalid");

    // The audit has each write.
    let audit = td.get("/api/audit?action=monitor.create").await;
    assert_eq!(audit.ok().as_array().unwrap().len(), 2);
    let audit = td.get("/api/audit?action=monitor.update").await;
    let changed = &audit.ok()[0]["detail"]["changed"];
    assert!(
        changed.as_array().unwrap().contains(&json!("thresholds")),
        "{changed}"
    );

    // Delete: a worker cannot; the lead can.
    delete_as(td, &worker, &format!("/api/monitors/{up}"))
        .await
        .err(403, "forbidden");
    let r = td.delete(&format!("/api/monitors/{up}")).await;
    assert_eq!(r.status, 204);
    td.get(&format!("/api/monitors/{up}"))
        .await
        .err(404, "not_found");
    assert_eq!(
        td.get("/api/audit?action=monitor.delete")
            .await
            .ok()
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn runs_status_events_and_the_dashboard() {
    let lab = Lab::start().await;
    let (lead, worker, _other, _h) = setup(&lab).await;
    let td = &lab.td;
    let path = "/api/projects/terra/monitors";
    let mut b = body("disk", "box", "echo 'used: 85.5%'", "number");
    b["unit"] = json!("%");
    b["warn"] = json!(80);
    b["crit"] = json!(90);
    let disk = id(td.post_as(&lead, path, b).await.ok());

    // A worker cannot run it; the lead can.
    td.post_empty_as(&worker, &format!("/api/monitors/{disk}/run"))
        .await
        .err(403, "forbidden");
    let r = td
        .post_empty_as(&lead, &format!("/api/monitors/{disk}/run"))
        .await;
    let v = r.ok();
    assert_eq!(v["result"]["status"], "warn", "{v}");
    assert_eq!(v["result"]["value"], 85.5);
    assert_eq!(v["output"]["exit_code"], 0);
    assert_eq!(v["output"]["stdout"], "used: 85.5%\n");
    // The status changes from none to warn, then to crit.
    td.patch(
        &format!("/api/monitors/{disk}"),
        json!({"script": "echo 95"}),
    )
    .await
    .ok();
    let v = td.post_empty(&format!("/api/monitors/{disk}/run")).await;
    assert_eq!(v.ok()["result"]["status"], "crit");
    // The same status again: a result, but no status event.
    td.post_empty(&format!("/api/monitors/{disk}/run"))
        .await
        .ok();
    let st = events(td, "monitor.status").await;
    assert_eq!(st.len(), 2, "{st:?}");
    assert_eq!(st[0]["data"]["from"], Value::Null);
    assert_eq!(st[0]["data"]["to"], "warn");
    assert_eq!(st[1]["data"]["from"], "warn");
    assert_eq!(st[1]["data"]["to"], "crit");
    assert_eq!(st[1]["data"]["monitor"], "disk");
    assert_eq!(st[1]["data"]["value"], 95.0);
    assert_eq!(st[1]["project"], "terra");
    assert_eq!(events(td, "monitor.result").await.len(), 3);
    // The project's agents see the events.
    let mine = td.get_as(&worker, "/api/events?kind=monitor.status").await;
    assert_eq!(mine.ok().as_array().unwrap().len(), 2);

    // Results, oldest first, and a downsample.
    let rs = td.get(&format!("/api/monitors/{disk}/results")).await;
    let rs = rs.ok().as_array().unwrap().clone();
    assert_eq!(rs.len(), 3);
    assert_eq!(rs[0]["value"], 85.5);
    let b1 = td
        .get(&format!("/api/monitors/{disk}/results?bucket=1h&since=2h"))
        .await;
    let pts = b1.ok().as_array().unwrap().clone();
    assert!(!pts.is_empty() && pts.len() <= 2, "{pts:?}");
    let total: u64 = pts.iter().map(|p| p["count"].as_u64().unwrap()).sum();
    assert_eq!(total, 3);
    td.get(&format!("/api/monitors/{disk}/results?bucket=soon"))
        .await
        .err(400, "bad_request");

    // A status monitor: exit 2 is crit, the first line the message.
    let st = id(td
        .post(
            path,
            body("svc", "box", "echo 'docker down'; exit 2", "status"),
        )
        .await
        .ok());
    let v = td.post_empty(&format!("/api/monitors/{st}/run")).await;
    assert_eq!(v.ok()["result"]["status"], "crit");
    assert_eq!(v.body["result"]["text"], "docker down");
    // A json monitor with rows.
    let js = id(td
        .post(
            path,
            body(
                "procs",
                "box",
                r#"echo '{"status":"ok","message":"2 rows","rows":[{"pid":1,"cmd":"init"},{"pid":2,"cmd":"sh"}]}'"#,
                "json",
            ),
        )
        .await
        .ok());
    let v = td.post_empty(&format!("/api/monitors/{js}/run")).await;
    assert_eq!(v.ok()["result"]["json"]["rows"][1]["cmd"], "sh");

    // The dashboard: a worker reads, cannot save; the lead saves.
    let dpath = "/api/projects/terra/dashboard";
    let d = td.get_as(&worker, dpath).await;
    assert_eq!(d.ok()["blocks"], json!([]));
    let db = td.daemon.as_ref().unwrap().state.db.clone();
    db.write("system", |t| {
        t.execute("INSERT INTO events(project,card,kind,at,payload) VALUES ('terra','broken','card','2026-11-08T00:00:00Z','not JSON')", [])?;
        Ok(())
    }).await.unwrap();
    let accuracy = td.get_as(&worker, "/api/projects/terra/eta-accuracy").await;
    assert_eq!(
        accuracy.ok(),
        json!({"raw": {"samples": 0, "on_time_pct": null, "within_half_to_double_pct": null}, "calibrated": {"samples": 0, "on_time_pct": null, "within_half_to_double_pct": null}})
    );
    td.get_as(&worker, dpath).await.ok();
    assert_eq!(d.body["monitors"].as_array().unwrap().len(), 3);
    let layout = json!({"blocks": [
        {"id": "h", "kind": "note", "text": "## Host", "size": "l"},
        {"id": "a", "kind": "stat", "monitor": disk, "options": {"decimals": 1}},
        {"id": "b", "kind": "chart", "monitor": disk, "size": "m", "options": {"range": "6h"}},
        {"id": "c", "kind": "status", "monitor": st},
        {"id": "d", "kind": "table", "monitor": js, "size": "l"},
    ]});
    td.put_as(&worker, dpath, layout.clone())
        .await
        .err(403, "forbidden");
    let d = td.put_as(&lead, dpath, layout).await;
    assert_eq!(d.ok()["blocks"].as_array().unwrap().len(), 5);
    assert_eq!(d.body["updated_by"], format!("agent:{lead}"));
    let disk_row = d.body["monitors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "disk")
        .unwrap()
        .clone();
    assert_eq!(disk_row["last"]["status"], "crit");
    // A stat block on a status monitor is refused, and names the block.
    let r = td
        .put(
            dpath,
            json!({"blocks": [{"id": "x", "kind": "stat", "monitor": st}]}),
        )
        .await;
    r.err(422, "invalid");
    assert_eq!(r.body["error"]["details"]["field"], "blocks[0].kind");
    assert_eq!(
        td.get("/api/audit?action=dashboard.update")
            .await
            .ok()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // Deleting a monitor drops its blocks.
    td.delete(&format!("/api/monitors/{disk}")).await;
    let d = td.get(dpath).await;
    assert_eq!(d.ok()["blocks"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn timeout_target_down_and_test_run() {
    let lab = Lab::start().await;
    let (lead, worker, _other, _h) = setup(&lab).await;
    let td = &lab.td;
    let path = "/api/projects/terra/monitors";

    // A run past its timeout is unknown.
    let mut b = body("slow", "box", "sleep 5; echo 1", "number");
    b["timeout_secs"] = json!(1);
    let slow = id(td.post(path, b).await.ok());
    let v = td.post_empty(&format!("/api/monitors/{slow}/run")).await;
    let r = &v.ok()["result"];
    assert_eq!(r["status"], "unknown");
    assert!(
        r["error"].as_str().unwrap().contains("timed out after 1 s"),
        "{r}"
    );

    // A target whose host agent is offline: unknown, target down.
    let far = id(td
        .post(path, body("far", "far", "echo 1", "number"))
        .await
        .ok());
    let v = td.post_empty(&format!("/api/monitors/{far}/run")).await;
    let r = &v.ok()["result"];
    assert_eq!(r["status"], "unknown");
    assert!(
        r["error"].as_str().unwrap().starts_with("target down"),
        "{r}"
    );
    assert!(v.body["output"].is_null());

    // A test run: nothing is kept; the raw output and the parse come back.
    let t = json!({"target": "box", "script": "echo 42; echo oops >&2", "kind": "number",
                   "warn": 40, "crit": 50});
    td.post_as(&worker, &format!("{path}/test"), t.clone())
        .await
        .err(403, "forbidden");
    let v = td.post_as(&lead, &format!("{path}/test"), t).await;
    let v = v.ok();
    assert_eq!(v["result"]["status"], "warn");
    assert_eq!(v["result"]["value"], 42.0);
    assert_eq!(v["output"]["stderr"], "oops\n");
    assert_eq!(
        td.get(path).await.ok().as_array().unwrap().len(),
        2,
        "a test run saves nothing"
    );
    assert_eq!(
        td.get("/api/audit?action=monitor.test")
            .await
            .ok()
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // A run_as that is gone: unknown, with the reason.
    let mine = id(td
        .post_as(&lead, path, body("mine", "box", "echo 1", "number"))
        .await
        .ok());
    let st = lab.state();
    st.db
        .write("system", move |t| {
            t.execute(
                "UPDATE monitors SET run_as = 'agent:01GONEGONEGONEGONEGONEGONE' WHERE id = ?1",
                [&mine],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let r = td.post_empty("/api/monitors/terra%2Fmine/run").await;
    assert_eq!(r.ok()["result"]["status"], "unknown");
    assert!(
        r.body["result"]["error"]
            .as_str()
            .unwrap()
            .contains("no longer exists")
    );
}

#[tokio::test]
async fn one_run_at_a_time_and_the_tick() {
    let lab = Lab::start().await;
    let (_lead, _worker, _other, _h) = setup(&lab).await;
    let td = &lab.td;
    let st = lab.state();
    let path = "/api/projects/terra/monitors";
    let m = id(td
        .post(path, body("sleeper", "box", "sleep 2; echo 7", "number"))
        .await
        .ok());
    // Enabled, with its next run hours ahead: only the fake clock runs it.
    let key = m.clone();
    let next = terrariumd::store::schedules::stamp(hours(2));
    st.db
        .write("system", move |t| {
            t.execute(
                "UPDATE monitors SET enabled = 1, next_run_at = ?2 WHERE id = ?1",
                [&key, &next],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let first = terrariumd::monitors::tick(&st, hours(3)).await;
    assert_eq!(first.len(), 1);
    assert!(st.monitors.is_running(&m));
    // The next tick finds the run still running: a skip, no second run.
    let second = terrariumd::monitors::tick(&st, hours(4)).await;
    assert!(second.is_empty());
    // A run now is refused while it runs.
    td.post_empty(&format!("/api/monitors/{m}/run"))
        .await
        .err(409, "conflict");
    let shown = td.get(&format!("/api/monitors/{m}")).await;
    assert_eq!(shown.ok()["skipped"], 1);
    assert_eq!(shown.body["running"], true);
    let (r, _) = first.into_iter().next().unwrap().await.unwrap().unwrap();
    assert_eq!(r.value, Some(7.0));
    assert!(!st.monitors.is_running(&m));
    // The next run moved past the fake now.
    let after = td.get(&format!("/api/monitors/{m}")).await;
    let next = after.ok()["next_run_at"].as_str().unwrap().to_owned();
    assert!(
        next > terrariumd::store::schedules::stamp(hours(4)),
        "{next}"
    );
}

#[tokio::test]
async fn retention_and_the_point_cap() {
    let lab = Lab::start().await;
    let (_lead, _worker, _other, _h) = setup(&lab).await;
    let td = &lab.td;
    let st = lab.state();
    // A project with no agents, so that it can be deleted at the end.
    td.project("solo").await;
    let mut b = body("old", "box", "echo 1", "number");
    b["retention_days"] = json!(2);
    let m = id(td.post("/api/projects/solo/monitors", b).await.ok());
    let key = m.clone();
    let n = terrariumd::store::monitors::MAX_POINTS + 5;
    let pruned = st
        .db
        .write("system", move |t| {
            let now = Utc::now();
            // Three days old: past the retention of two days.
            for i in 0..3 {
                let at = terrariumd::store::schedules::stamp(
                    now - chrono::Duration::days(3) + chrono::Duration::seconds(i),
                );
                t.execute(
                    "INSERT INTO monitor_results (monitor, at, status, value, duration_ms)
                     VALUES (?1, ?2, 'ok', 1, 1)",
                    rusqlite::params![key, at],
                )?;
            }
            // Recent: past the point cap.
            for i in 0..n {
                let at =
                    terrariumd::store::schedules::stamp(now - chrono::Duration::seconds(n - i));
                t.execute(
                    "INSERT INTO monitor_results (monitor, at, status, value, duration_ms)
                     VALUES (?1, ?2, 'ok', ?3, 1)",
                    rusqlite::params![key, at, i as f64],
                )?;
            }
            terrariumd::store::monitors::prune(t, &key, 2, now)
        })
        .await
        .unwrap();
    assert_eq!(pruned, 3 + 5);
    let key = m.clone();
    let (count, oldest): (i64, f64) = st
        .db
        .read(move |c| {
            Ok(c.query_row(
                "SELECT COUNT(*), MIN(value) FROM monitor_results WHERE monitor = ?1",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(count, terrariumd::store::monitors::MAX_POINTS);
    // The oldest five went.
    assert_eq!(oldest, 5.0);
    // A limit keeps the newest, oldest first.
    let r = td.get(&format!("/api/monitors/{m}/results?limit=3")).await;
    let vals: Vec<f64> = r
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["value"].as_f64().unwrap())
        .collect();
    let top = (n - 1) as f64;
    assert_eq!(vals, [top - 2.0, top - 1.0, top]);
    // Deleting the project deletes its monitors and results.
    let r = td.delete("/api/projects/solo?confirm=true").await;
    assert!(r.status.is_success(), "{}", r.body);
    let key = m.clone();
    let left: i64 = st
        .db
        .read(move |c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM monitor_results WHERE monitor = ?1",
                [&key],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(left, 0);
}

// ------------------------------------------------ owners (issue #18, §30.7)

async fn audit_rows(td: &TestDaemon, action: &str) -> Vec<Value> {
    td.get(&format!("/api/audit?action={action}"))
        .await
        .ok()
        .as_array()
        .unwrap()
        .clone()
}

#[tokio::test]
async fn project_owned_monitors_rights_grants_and_the_audit_actor() {
    let lab = Lab::start().await;
    let (lead, worker, other, _h) = setup(&lab).await;
    let td = &lab.td;
    let path = "/api/projects/terra/monitors";
    // A target of the project; the lead has it in no policy and no lease.
    lab.target(json!({"name": "terra-tbox", "kind": "host", "host": "lab1", "project": "terra"}))
        .await;
    let owned = |name: &str, target: &str, owner: &str| {
        let mut b = body(name, target, "echo 42", "number");
        b["owner"] = json!(owner);
        b
    };

    // A worker agent must not create a project-owned monitor.
    td.post_as(&worker, path, owned("w", "terra-tbox", "project"))
        .await
        .err(403, "forbidden");
    // Nor a lead of another project.
    td.post_as(&other, path, owned("o", "terra-tbox", "project"))
        .await
        .err(403, "forbidden");
    // The lead makes one on a project target: the project's right, not
    // the lead's policy.
    let r = td
        .post_as(&lead, path, owned("ptarget", "terra-tbox", "project"))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["owner"], "project");
    let ptarget = id(&r.body);
    // A global target needs a grant, even when it is in the lead's policy.
    let r = td
        .post_as(&lead, path, owned("pbox", "box", "project"))
        .await;
    r.err(422, "invalid");
    assert_eq!(r.body["error"]["details"]["field"], "target");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not granted"),
        "{}",
        r.body
    );
    // A target of another project: never.
    td.post(path, owned("pops", "opsbox", "project"))
        .await
        .err(422, "invalid");

    // Only a sysadmin or the operator grants a global target.
    let grants = "/api/projects/terra/target-grants";
    td.post_as(&lead, grants, json!({"target": "box", "for": "monitors"}))
        .await
        .err(403, "forbidden");
    td.post(grants, json!({"target": "terra-tbox", "for": "monitors"}))
        .await
        .err(422, "invalid");
    td.post(grants, json!({"target": "box", "for": "pies"}))
        .await
        .err(422, "invalid");
    let r = td
        .post(grants, json!({"target": "box", "for": "monitors"}))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["for"], "monitors");
    assert_eq!(r.body["granted_by"], "operator");
    let r = td
        .post(grants, json!({"target": "box", "for": "monitors"}))
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    // The grant is visible to the members.
    let list = td.get_as(&worker, grants).await;
    assert_eq!(list.ok().as_array().unwrap().len(), 1);
    assert_eq!(list.body[0]["target"], "box");
    assert_eq!(audit_rows(td, "target.grant").await.len(), 1);
    // A grant for schedules is not one for monitors.
    let r = td
        .post_as(&lead, path, owned("pbox", "box", "project"))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let pbox = id(&r.body);

    // System-owned: sysadmin or the operator.
    td.post_as(&lead, path, owned("sys", "far", "system"))
        .await
        .err(403, "forbidden");
    let r = td.post(path, owned("sys", "far", "system")).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let sys = id(&r.body);
    // The lead does not change, run, or delete it.
    td.patch_as(&lead, &format!("/api/monitors/{sys}"), json!({"unit": "x"}))
        .await
        .err(403, "forbidden");
    td.post_empty_as(&lead, &format!("/api/monitors/{sys}/run"))
        .await
        .err(403, "forbidden");
    delete_as(td, &lead, &format!("/api/monitors/{sys}"))
        .await
        .err(403, "forbidden");
    // Nor takes it over.
    td.patch_as(
        &lead,
        &format!("/api/monitors/{sys}"),
        json!({"owner": "actor"}),
    )
    .await
    .err(403, "forbidden");

    // The runs: the result rows carry the monitor as the actor. No run
    // writes a target.exec row; the first status (none -> ok) writes one
    // monitor.status audit row with the monitor as the actor.
    for m in [&ptarget, &pbox] {
        let r = td
            .post_empty_as(&lead, &format!("/api/monitors/{m}/run"))
            .await;
        assert_eq!(r.ok()["result"]["status"], "ok", "{}", r.body);
        assert_eq!(r.body["result"]["value"], 42.0);
    }
    let res = td.get(&format!("/api/monitors/{pbox}/results")).await;
    assert_eq!(res.ok()[0]["actor"], "monitor:terra/pbox", "{}", res.body);
    assert!(audit_rows(td, "target.exec").await.is_empty());
    let rows = audit_rows(td, "monitor.status").await;
    let row = rows
        .iter()
        .find(|r| r["actor"] == "monitor:terra/pbox")
        .expect("a monitor.status audit row");
    assert_eq!(row["detail"]["to"], "ok");
    assert_eq!(row["detail"]["owner"], "project");
    assert_eq!(row["detail"]["id"], pbox.as_str());
    // A second run with the same status writes no audit row.
    td.post_empty(&format!("/api/monitors/{pbox}/run"))
        .await
        .ok();
    let n = |rows: &[Value]| {
        rows.iter()
            .filter(|r| r["actor"] == "monitor:terra/pbox")
            .count()
    };
    assert_eq!(n(&audit_rows(td, "monitor.status").await), 1);
    // The result events show the monitor too.
    let evs = events(td, "monitor.result").await;
    assert!(
        evs.iter().any(|e| e["actor"] == "monitor:terra/pbox"),
        "{evs:?}"
    );

    // The lead goes: a project-owned monitor still runs.
    let r = td.delete(&format!("/api/agents/{lead}")).await;
    assert_eq!(r.status, 204, "{}", r.body);
    let r = td.post_empty(&format!("/api/monitors/{pbox}/run")).await;
    assert_eq!(r.ok()["result"]["status"], "ok", "{}", r.body);
    let m = td.get(&format!("/api/monitors/{pbox}")).await;
    assert_eq!(m.ok()["owner"], "project");
    assert!(m.body["disabled_reason"].is_null());

    // A revoked grant refuses the next run.
    let r = td.delete(&format!("{grants}/box/monitors")).await;
    assert_eq!(r.status, 204, "{}", r.body);
    assert_eq!(audit_rows(td, "target.ungrant").await.len(), 1);
    let r = td.post_empty(&format!("/api/monitors/{pbox}/run")).await;
    assert_eq!(r.ok()["result"]["status"], "unknown", "{}", r.body);
    assert!(
        r.body["result"]["error"]
            .as_str()
            .unwrap()
            .contains("not granted"),
        "{}",
        r.body
    );
    // ok -> unknown is a status change: one more audit row.
    let rows = audit_rows(td, "monitor.status").await;
    assert_eq!(n(&rows), 2);
    assert!(rows.iter().any(|r| r["actor"] == "monitor:terra/pbox"
        && r["detail"]["from"] == "ok"
        && r["detail"]["to"] == "unknown"));
}

#[tokio::test]
async fn moving_a_monitor_to_the_project_is_one_edit() {
    let lab = Lab::start().await;
    let (lead, worker, _other, _h) = setup(&lab).await;
    let td = &lab.td;
    let path = "/api/projects/terra/monitors";
    lab.target(json!({"name": "terra-tbox", "kind": "host", "host": "lab1", "project": "terra"}))
        .await;
    let r = td
        .post(path, body("load", "terra-tbox", "echo 1", "number"))
        .await;
    let load = id(r.ok());
    assert_eq!(r.body["owner"], "actor");
    assert_eq!(r.body["run_as"], "operator");
    // A worker cannot move it.
    td.patch_as(
        &worker,
        &format!("/api/monitors/{load}"),
        json!({"owner": "project"}),
    )
    .await
    .err(403, "forbidden");
    let r = td
        .patch_as(
            &lead,
            &format!("/api/monitors/{load}"),
            json!({"owner": "project"}),
        )
        .await;
    assert_eq!(r.ok()["owner"], "project", "{}", r.body);
    let audit = audit_rows(td, "monitor.update").await;
    let last = audit.last().unwrap();
    assert!(
        last["detail"]["changed"]
            .as_array()
            .unwrap()
            .contains(&json!("owner")),
        "{last}"
    );
    assert_eq!(last["actor"], format!("agent:{lead}"));
    // Back to an actor: the caller takes it over and must use the target.
    td.patch_as(
        &lead,
        &format!("/api/monitors/{load}"),
        json!({"owner": "actor"}),
    )
    .await
    .err(403, "forbidden");
    let r = td
        .patch(&format!("/api/monitors/{load}"), json!({"owner": "actor"}))
        .await;
    assert_eq!(r.ok()["owner"], "actor");
    assert_eq!(r.body["run_as"], "operator");
}

#[tokio::test]
async fn a_deleted_run_as_agent_disables_its_monitors_and_schedules() {
    let lab = Lab::start().await;
    let (lead, _worker, _other, _h) = setup(&lab).await;
    let td = &lab.td;
    let lead2 = id(&td
        .agent(
            "terra",
            "lead2",
            json!({"roles": ["project_admin"], "policy": {"targets": ["box"]}}),
        )
        .await);
    // An enabled monitor and an enabled schedule that run as the lead.
    let mut b = body("load", "box", "echo 1", "number");
    b["enabled"] = json!(true);
    b["schedule"] = json!({"kind": "cron", "expr": "0 0 1 1 *"});
    let r = td.post_as(&lead, "/api/projects/terra/monitors", b).await;
    let load = id(r.ok());
    // A disabled one stays as it is.
    let r = td
        .post_as(
            &lead,
            "/api/projects/terra/monitors",
            body("off", "box", "echo 1", "number"),
        )
        .await;
    let off = id(r.ok());
    let r = td
        .post_as(
            &lead,
            "/api/schedules",
            json!({"name": "nag", "project": "terra",
                   "spec": {"kind": "interval", "every": "1h"},
                   "action": {"type": "mail", "to": "operator", "body": "hi"}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let nag = id(&r.body);
    assert_eq!(r.body["owner"], "actor");

    let r = td.delete(&format!("/api/agents/{lead}")).await;
    assert_eq!(r.status, 204, "{}", r.body);

    let m = td.get(&format!("/api/monitors/{load}")).await;
    assert_eq!(m.ok()["enabled"], false, "{}", m.body);
    assert!(m.body["next_run_at"].is_null());
    let why = m.body["disabled_reason"].as_str().unwrap();
    assert!(why.contains("terra/lead was deleted"), "{why}");
    let m = td.get(&format!("/api/monitors/{off}")).await;
    assert!(m.ok()["disabled_reason"].is_null());
    let s = td.get(&format!("/api/schedules/{nag}")).await;
    assert_eq!(s.ok()["enabled"], false, "{}", s.body);
    assert!(
        s.body["disabled_reason"]
            .as_str()
            .unwrap()
            .contains("was deleted")
    );
    assert_eq!(audit_rows(td, "monitor.disable").await.len(), 1);

    // The notice: the operator's Inbox and the other lead.
    let ops = td.get("/api/mail?to=operator").await;
    let notice = ops
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["from"] == "system")
        .cloned()
        .expect("a notice to the operator");
    let subject = notice["subject"].as_str().unwrap();
    assert!(subject.contains("terra/lead was deleted"), "{subject}");
    let body_text = notice["body"].as_str().unwrap();
    assert!(body_text.contains("monitor terra/load"), "{body_text}");
    assert!(body_text.contains("schedule terra/nag"), "{body_text}");
    let inbox = td.get("/api/inbox?kind=mail").await;
    assert!(
        inbox
            .ok()
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i.to_string().contains("was deleted")),
        "{}",
        inbox.body
    );
    let to_lead2 = td.get(&format!("/api/mail?to={lead2}")).await;
    assert!(
        to_lead2
            .ok()
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["from"] == "system"),
        "{}",
        to_lead2.body
    );

    // A run of the orphan says what to do.
    let r = td.post_empty(&format!("/api/monitors/{off}/run")).await;
    let err = r.ok()["result"]["error"].as_str().unwrap().to_owned();
    assert!(err.contains("take it over"), "{err}");

    // Take-over: the other lead runs it as itself and enables it.
    let r = td
        .patch_as(
            &lead2,
            &format!("/api/monitors/{load}"),
            json!({"owner": "actor", "enabled": true}),
        )
        .await;
    assert_eq!(r.ok()["run_as"], format!("agent:{lead2}"), "{}", r.body);
    assert_eq!(r.body["enabled"], true);
    assert!(r.body["disabled_reason"].is_null());
    // An enable of an orphan schedule takes it over too.
    let r = td
        .post_empty_as(&lead2, &format!("/api/schedules/{nag}/enable"))
        .await;
    assert_eq!(r.ok()["run_as"], format!("agent:{lead2}"), "{}", r.body);
    assert_eq!(r.body["enabled"], true);
    assert!(r.body["disabled_reason"].is_null());
}

#[tokio::test]
async fn project_owned_schedules_exec_with_the_project_rights() {
    let lab = Lab::start().await;
    let (lead, worker, _other, _h) = setup(&lab).await;
    let td = &lab.td;
    lab.target(json!({"name": "terra-tbox", "kind": "host", "host": "lab1", "project": "terra"}))
        .await;
    let sched = |name: &str, target: &str, owner: &str| {
        json!({"name": name, "project": "terra", "owner": owner, "enabled": false,
               "spec": {"kind": "interval", "every": "1h"},
               "action": {"type": "exec", "target": target, "cmd": ["echo", "hi"]}})
    };
    // A worker must not create a project-owned schedule.
    td.post_as(
        &worker,
        "/api/schedules",
        sched("w", "terra-tbox", "project"),
    )
    .await
    .err(403, "forbidden");
    // A global target needs a grant for schedules; one for monitors is not it.
    td.post(
        "/api/projects/terra/target-grants",
        json!({"target": "box", "for": "monitors"}),
    )
    .await;
    td.post_as(&lead, "/api/schedules", sched("g", "box", "project"))
        .await
        .err(403, "forbidden");
    // A system-owned schedule needs sysadmin or the operator.
    td.post_as(&lead, "/api/schedules", sched("s", "terra-tbox", "system"))
        .await
        .err(403, "forbidden");
    // A global schedule cannot be project-owned.
    let mut g = sched("gl", "box", "project");
    g["project"] = Value::Null;
    td.post("/api/schedules", g).await.err(422, "invalid");

    let r = td
        .post_as(
            &lead,
            "/api/schedules",
            sched("ping", "terra-tbox", "project"),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["owner"], "project");
    let ping = id(&r.body);
    // The lead goes; the schedule runs with the project's rights.
    let r = td.delete(&format!("/api/agents/{lead}")).await;
    assert_eq!(r.status, 204, "{}", r.body);
    let s = td.get(&format!("/api/schedules/{ping}")).await;
    assert!(s.ok()["disabled_reason"].is_null());
    let r = td.post_empty(&format!("/api/schedules/{ping}/run")).await;
    assert_eq!(r.ok()["status"], "ok", "{}", r.body);
    let rows = audit_rows(td, "target.exec").await;
    let row = rows
        .iter()
        .find(|r| r["target"] == "terra-tbox")
        .expect("a target.exec row");
    assert_eq!(row["actor"], format!("schedule:{ping}"));
    assert_eq!(row["detail"]["owner"], "project");
}
