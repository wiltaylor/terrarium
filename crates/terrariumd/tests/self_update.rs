//! The self-update (docs/api.md §34): the rights of `/api/system/update`
//! (the operator and sysadmins, agents with the role too; a worker agent
//! is refused), the audit rows, the drain, the result and release mails,
//! and the `auto` policy that updates only inside a global maintenance
//! window, once. A fake updater answers on a socket in the test dir; no
//! Docker and no network.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{Duration, Utc};
use common::TestDaemon;
use serde_json::{Value, json};
use terrarium_core::AgentId;
use terrarium_updater::protocol::{
    Image, Outcome, Request, Response, Run, RunKind, Status, Step, StepState,
};
use terrariumd::lifecycle::Woke;
use terrariumd::self_update::{self, Channel, Config, Policy, Release};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// The fake updater: it records each request and answers from its status.
#[derive(Default)]
struct FakeUpdater {
    requests: Mutex<Vec<Request>>,
    status: Mutex<Option<Status>>,
}

impl FakeUpdater {
    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    fn set_history(&self, runs: Vec<Run>) {
        let mut s = self.status.lock().unwrap();
        s.get_or_insert_with(status).history = runs;
    }
}

fn status() -> Status {
    Status {
        updater_version: "0.1.0+test".into(),
        repository: "ghcr.io/wiltaylor/terrarium".into(),
        verify: "gh".into(),
        current: Some(Image {
            reference: "terrarium:dev".into(),
            id: "sha256:1".into(),
            digest: None,
            version: Some("0.1.0".into()),
        }),
        current_error: None,
        previous: None,
        running: None,
        history: Vec::new(),
    }
}

fn run(id: &str, outcome: Outcome) -> Run {
    Run {
        id: id.into(),
        kind: RunKind::Update,
        actor: "operator".into(),
        target: "99.0.0".into(),
        from: None,
        to: Some(Image {
            reference: "ghcr.io/wiltaylor/terrarium@sha256:9".into(),
            id: "sha256:9".into(),
            digest: Some("sha256:9".into()),
            version: Some("99.0.0".into()),
        }),
        backup: Some("/state/backups/b.zip".into()),
        outcome,
        error: (outcome == Outcome::RolledBack).then(|| "health: unhealthy".into()),
        steps: vec![Step {
            name: "health".into(),
            state: StepState::Ok,
            detail: "healthy".into(),
            at: "2026-11-04T00:00:00Z".into(),
        }],
        started_at: "2026-11-04T00:00:00Z".into(),
        ended_at: Some("2026-11-04T00:05:00Z".into()),
    }
}

/// Serve the fake updater on `path`.
fn serve(path: PathBuf) -> Arc<FakeUpdater> {
    let fake = Arc::new(FakeUpdater::default());
    *fake.status.lock().unwrap() = Some(status());
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let f = fake.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut line = String::new();
            BufReader::new(read).read_line(&mut line).await.unwrap();
            let req: Request = serde_json::from_str(line.trim()).unwrap();
            f.requests.lock().unwrap().push(req.clone());
            let answer = match req {
                Request::Status => {
                    Response::Status(Box::new(f.status.lock().unwrap().clone().unwrap()))
                }
                Request::Update { .. } | Request::Rollback { .. } => Response::Accepted {
                    run: "01RUN".into(),
                },
            };
            let mut out = serde_json::to_vec(&answer).unwrap();
            out.push(b'\n');
            write.write_all(&out).await.unwrap();
        }
    });
    fake
}

fn state(td: &TestDaemon) -> &terrariumd::api::Shared {
    &td.daemon.as_ref().unwrap().state
}

/// Point the daemon at a fake updater in the test dir.
fn with_updater(td: &TestDaemon, policy: Policy) -> Arc<FakeUpdater> {
    let path = td.dir.path().join("updater.sock");
    let fake = serve(path.clone());
    state(td).self_update.set_config(Config {
        policy,
        channel: Channel::Stable,
        socket: path,
        releases_url: "http://127.0.0.1:9/never".into(),
    });
    fake
}

fn release(version: &str) -> Release {
    Release {
        version: version.into(),
        tag: format!("v{version}"),
        name: format!("v{version}"),
        prerelease: false,
        published_at: None,
        notes: "notes".into(),
        url: format!("https://github.com/wiltaylor/terrarium/releases/tag/v{version}"),
    }
}

async fn operator_mail(td: &TestDaemon) -> Vec<(String, String)> {
    state(td)
        .db
        .read(|c| terrariumd::store::mail::operator_mail(c, true, 100))
        .await
        .unwrap()
        .into_iter()
        .map(|m| (m.subject, m.body))
        .collect()
}

async fn audit_actions(td: &TestDaemon) -> Vec<(String, String)> {
    td.get("/api/audit?limit=100")
        .await
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["action"].as_str().unwrap_or_default().to_owned(),
                r["actor"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn sysadmin_agents_and_the_operator_update_and_a_worker_is_refused() {
    let td = TestDaemon::start().await;
    let fake = with_updater(&td, Policy::Notify);
    td.project("ops").await;
    let worker = id(&td.agent("ops", "dev", json!({})).await);
    let sys = id(&td
        .agent("ops", "root", json!({"roles": ["sysadmin"]}))
        .await);

    // A worker agent reads nothing and changes nothing.
    td.get_as(&worker, "/api/system/update")
        .await
        .err(403, "forbidden");
    for path in [
        "/api/system/update",
        "/api/system/update/check",
        "/api/system/update/rollback",
    ] {
        td.post_as(&worker, path, json!({}))
            .await
            .err(403, "forbidden");
    }
    assert!(fake.requests().is_empty());

    // The sysadmin agent reads, updates, and rolls back.
    let r = td.get_as(&sys, "/api/system/update").await;
    let b = r.ok();
    assert_eq!(b["policy"], "notify");
    assert_eq!(b["channel"], "stable");
    assert_eq!(b["updater"]["reachable"], true);
    assert_eq!(b["updater"]["status"]["verify"], "gh");
    assert_eq!(b["current"]["cargo_version"], terrarium_version::VERSION);
    assert_eq!(b["drain"]["on"], false);

    let r = td
        .post_as(&sys, "/api/system/update", json!({"version": "v99.0.0"}))
        .await;
    assert_eq!(r.status, 202, "{}", r.body);
    assert_eq!(r.body["run"], "01RUN");
    let r = td
        .post_as(&sys, "/api/system/update/rollback", json!({}))
        .await;
    assert_eq!(r.status, 202, "{}", r.body);
    // The operator updates to the channel tag when no release is known.
    let r = td.post_empty("/api/system/update").await;
    assert_eq!(r.status, 202, "{}", r.body);
    assert_eq!(r.body["target"], "latest");
    // A bad version never reaches the updater.
    td.post("/api/system/update", json!({"version": "1.0; rm"}))
        .await
        .err(422, "invalid");

    let updates: Vec<Request> = fake
        .requests()
        .into_iter()
        .filter(|r| *r != Request::Status)
        .collect();
    assert_eq!(
        updates,
        [
            Request::Update {
                target: "v99.0.0".into(),
                actor: format!("agent:{sys}")
            },
            Request::Rollback {
                actor: format!("agent:{sys}")
            },
            Request::Update {
                target: "latest".into(),
                actor: "operator".into()
            },
        ]
    );
    let audit = audit_actions(&td).await;
    assert!(
        audit.contains(&("system.update".into(), format!("agent:{sys}"))),
        "{audit:?}"
    );
    assert!(audit.contains(&("system.rollback".into(), format!("agent:{sys}"))));
    assert!(audit.contains(&("system.update".into(), "operator".into())));

    // The drain is for the updater: not even a sysadmin agent.
    td.post_as(&sys, "/api/system/update/drain", json!({}))
        .await
        .err(403, "forbidden");
}

#[tokio::test]
async fn no_updater_is_service_unavailable() {
    let td = TestDaemon::start().await;
    state(&td).self_update.set_config(Config {
        socket: td.dir.path().join("none.sock"),
        ..Config::default()
    });
    let r = td.post_empty("/api/system/update").await;
    r.err(503, "unavailable");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("terrarium-updater"),
        "{}",
        r.body
    );
    let b = td.get("/api/system/update").await;
    assert_eq!(b.ok()["updater"]["reachable"], false);
}

#[tokio::test]
async fn the_drain_holds_wakes_and_closes_the_stream_gate_until_resume() {
    let td = TestDaemon::start().await;
    td.project("ops").await;
    let a = td.agent("ops", "dev", json!({})).await;
    let agent: AgentId = id(&a).parse().unwrap();
    let st = state(&td).clone();

    let r = td.post_empty("/api/system/update/drain").await;
    assert_eq!(r.ok()["on"], true);
    assert!(st.usage.streams.is_closed());
    assert_eq!(
        terrariumd::lifecycle::wake(&st, agent, "mail").await,
        Woke::Draining
    );
    let b = td.get("/api/system/update").await;
    assert_eq!(b.ok()["drain"]["on"], true);
    assert_eq!(b.ok()["drain"]["by"], "operator");
    assert!(
        audit_actions(&td)
            .await
            .contains(&("system.drain".into(), "operator".into()))
    );

    let r = td.post_empty("/api/system/update/resume").await;
    assert_eq!(r.ok()["on"], false);
    assert!(!st.usage.streams.is_closed());
    assert_ne!(
        terrariumd::lifecycle::wake(&st, agent, "mail").await,
        Woke::Draining
    );
}

#[tokio::test]
async fn a_new_release_and_each_result_mail_the_operator_once() {
    let td = TestDaemon::start().await;
    let fake = with_updater(&td, Policy::Notify);
    let st = state(&td).clone();
    let now = Utc::now();
    // An old run in the history at the first read: no mail.
    fake.set_history(vec![run("old", Outcome::Succeeded)]);
    st.self_update.set_releases(vec![release("99.0.0")], now);

    let t = self_update::tick(&st, now, false).await;
    assert_eq!(t.release_mailed.as_deref(), Some("99.0.0"));
    assert!(t.results_mailed.is_empty());
    assert_eq!(t.auto_update, None, "notify does not update");
    let mail = operator_mail(&td).await;
    assert_eq!(mail.len(), 1);
    assert_eq!(mail[0].0, "Terrarium 99.0.0 is available");
    assert!(
        mail[0]
            .1
            .contains("terrarium admin update apply --version 99.0.0")
    );

    // A new result: one mail; the next tick sends nothing.
    fake.set_history(vec![
        run("new", Outcome::RolledBack),
        run("old", Outcome::Succeeded),
    ]);
    let t = self_update::tick(&st, now, false).await;
    assert_eq!(t.release_mailed, None, "once per version");
    assert_eq!(t.results_mailed, ["new"]);
    let mail = operator_mail(&td).await;
    assert_eq!(mail.len(), 2);
    let (subject, body) = mail
        .iter()
        .find(|m| m.0.contains("rolled back"))
        .unwrap()
        .clone();
    assert_eq!(
        subject,
        "Terrarium update to 99.0.0 failed and was rolled back"
    );
    assert!(body.contains("health: unhealthy"), "{body}");
    assert_eq!(self_update::tick(&st, now, false).await, Default::default());

    // `off`: no mail, no check.
    let mut cfg = st.self_update.config();
    cfg.policy = Policy::Off;
    st.self_update.set_config(cfg);
    st.self_update.set_releases(vec![release("100.0.0")], now);
    assert_eq!(self_update::tick(&st, now, false).await, Default::default());
}

#[tokio::test]
async fn auto_updates_only_inside_a_global_maintenance_window_once() {
    let td = TestDaemon::start().await;
    let fake = with_updater(&td, Policy::Auto);
    let st = state(&td).clone();
    let now = Utc::now();
    st.self_update.set_releases(vec![release("99.0.0")], now);
    let updates = |f: &FakeUpdater| {
        f.requests()
            .into_iter()
            .filter(|r| matches!(r, Request::Update { .. }))
            .count()
    };

    // No window: a mail, no update.
    let t = self_update::tick(&st, now, false).await;
    assert_eq!(t.release_mailed.as_deref(), Some("99.0.0"));
    assert_eq!(t.auto_update, None);

    // A project maintenance event does not count; a global window in the
    // future neither.
    td.project("ops").await;
    let ev = |start: chrono::DateTime<Utc>, kind: &str| {
        json!({"title": "patch night", "kind": kind,
               "start": terrariumd::time_param::stored(start),
               "end": terrariumd::time_param::stored(start + Duration::hours(2))})
    };
    let r = td
        .post(
            "/api/projects/ops/calendar/events",
            ev(now - Duration::minutes(30), "maintenance"),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let r = td
        .post(
            "/api/calendar/global/events",
            ev(now + Duration::hours(3), "maintenance"),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let r = td
        .post(
            "/api/calendar/global/events",
            ev(now - Duration::minutes(30), "freeze"),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(self_update::tick(&st, now, false).await.auto_update, None);
    assert_eq!(updates(&fake), 0);

    // A global maintenance window now: one update in it.
    let r = td
        .post(
            "/api/calendar/global/events",
            ev(now - Duration::minutes(30), "maintenance"),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let w = self_update::current_window(&st, now).await.unwrap();
    assert_eq!(w.title, "patch night");
    let t = self_update::tick(&st, now, false).await;
    assert_eq!(t.auto_update.as_deref(), Some("01RUN"));
    assert_eq!(
        fake.requests()
            .into_iter()
            .find(|r| matches!(r, Request::Update { .. }))
            .unwrap(),
        Request::Update {
            target: "99.0.0".into(),
            actor: self_update::AUTO_ACTOR.into()
        }
    );
    // At most once for each window.
    assert_eq!(self_update::tick(&st, now, false).await.auto_update, None);
    assert_eq!(updates(&fake), 1);
    assert!(
        audit_actions(&td)
            .await
            .contains(&("system.update".into(), self_update::AUTO_ACTOR.into()))
    );
    let b = td.get("/api/system/update").await;
    assert_eq!(b.ok()["maintenance"]["title"], "patch night");
    assert_eq!(b.ok()["update_available"], true);
    assert_eq!(b.ok()["latest"]["version"], "99.0.0");
}
