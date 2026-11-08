//! Pipelines (docs/api.md §31): the rights, the API, the GitHub adapter
//! against a fake GitHub API on 127.0.0.1, the script adapter on a real
//! host agent (`terrarium_host::Client` on this machine), the events, the
//! secrets, and the tick. The tick tests run at times hours ahead of the
//! real clock, so the daemon's own timer never polls these pipelines.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use common::TestDaemon;
use serde_json::{Value, json};
use terrarium_core::protocol::HostCapabilities;
use terrarium_host::{Client, ClientOptions};
use terrariumd::api::Shared;

const GH_TOKEN: &str = "ghp_T3rr4r1umP1p3l1n3sT0k3nX9";

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

fn hours(n: i64) -> DateTime<Utc> {
    Utc::now() + chrono::Duration::hours(n)
}

async fn events(td: &TestDaemon, kind: &str) -> Vec<Value> {
    td.get(&format!("/api/events?kind={kind}"))
        .await
        .ok()
        .as_array()
        .unwrap()
        .clone()
}

// ------------------------------------------------------------ fake GitHub

/// One answer of the fake GitHub API: the status, the headers, the body.
type Answer = (u16, Vec<(&'static str, String)>, Value);

/// The fake GitHub API: an answer for each path, and the requests seen.
#[derive(Clone, Default)]
struct Gh {
    answers: Arc<Mutex<HashMap<String, Answer>>>,
    seen: Arc<Mutex<Vec<(String, HeaderMap)>>>,
}

impl Gh {
    fn answer(&self, path: &str, status: u16, body: Value) {
        self.answers
            .lock()
            .unwrap()
            .insert(path.to_owned(), (status, Vec::new(), body));
    }

    fn answer_with(
        &self,
        path: &str,
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: Value,
    ) {
        self.answers
            .lock()
            .unwrap()
            .insert(path.to_owned(), (status, headers, body));
    }

    fn seen(&self) -> Vec<(String, HeaderMap)> {
        self.seen.lock().unwrap().clone()
    }
}

async fn gh_answer(State(g): State<Gh>, uri: Uri, headers: HeaderMap) -> Response {
    let pq = uri
        .path_and_query()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_default();
    g.seen.lock().unwrap().push((pq, headers));
    let a = g.answers.lock().unwrap().get(uri.path()).cloned();
    match a {
        None => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"message": "Not Found"})),
        )
            .into_response(),
        Some((status, extra, body)) => {
            let mut r = (StatusCode::from_u16(status).unwrap(), axum::Json(body)).into_response();
            for (k, v) in extra {
                r.headers_mut().insert(k, v.parse().unwrap());
            }
            r
        }
    }
}

async fn serve_gh() -> (Gh, String) {
    let g = Gh::default();
    let app = Router::new().fallback(gh_answer).with_state(g.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (g, format!("http://127.0.0.1:{port}"))
}

fn gh_run(id: u64, status: &str, conclusion: Option<&str>, started: &str) -> Value {
    json!({"id": id, "name": "CI", "run_number": id, "status": status,
           "conclusion": conclusion, "html_url": format!("https://github.com/o/r/actions/runs/{id}"),
           "head_branch": "main", "head_sha": format!("sha{id}"), "event": "push",
           "display_title": format!("commit {id}"), "actor": {"login": "wil"},
           "run_started_at": started, "created_at": started, "updated_at": started})
}

const RUNS: &str = "/repos/o/r/actions/workflows/ci.yml/runs";

fn jobs_path(id: u64) -> String {
    format!("/repos/o/r/actions/runs/{id}/jobs")
}

/// Project `terra` with a lead (project_admin) and a worker; project
/// `ops` with an agent; secret `gh-token` of `terra`.
async fn setup(td: &TestDaemon) -> (String, String, String) {
    td.project("terra").await;
    td.project("ops").await;
    let lead = td
        .agent("terra", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let worker = td.agent("terra", "worker", json!({})).await;
    let other = td.agent("ops", "dev", json!({})).await;
    td.put(
        "/api/secrets/terra/gh-token",
        json!({"kind": "github_token", "fields": {"value": GH_TOKEN}}),
    )
    .await
    .ok();
    (id(&lead), id(&worker), id(&other))
}

fn gh_body(name: &str, api: &str) -> Value {
    json!({"name": name, "enabled": false,
           "source": {"kind": "github", "repo": "o/r", "workflow": "ci.yml",
                      "token_secret": "gh-token", "api_url": api}})
}

#[tokio::test]
async fn rights_crud_and_validation() {
    let td = TestDaemon::start().await;
    let (lead, worker, other) = setup(&td).await;
    let path = "/api/projects/terra/pipelines";
    let body = gh_body("ci", "https://api.github.com");

    // A worker may not create; the lead may.
    td.post_as(&worker, path, body.clone())
        .await
        .err(403, "forbidden");
    let r = td.post_as(&lead, path, body.clone()).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let p = r.body.clone();
    assert_eq!(p["source"]["kind"], "github");
    assert_eq!(p["schedule"], json!({"kind": "interval", "every": "5m"}));
    assert_eq!(p["max_runs"], 20);
    assert_eq!(p["run_as"], format!("agent:{lead}"));
    assert_eq!(p["next_run_at"], Value::Null, "disabled");
    assert_eq!(p["status"], Value::Null);
    assert_eq!(p["running"], false);
    // A name in use is a conflict.
    td.post(path, body.clone()).await.err(409, "conflict");

    // Members read; another project's agent does not.
    let l = td.get_as(&worker, path).await.ok().clone();
    assert_eq!(l.as_array().unwrap().len(), 1);
    td.get_as(&other, path).await.err(403, "forbidden");
    let by_key = td.get("/api/pipelines/terra%2Fci").await.ok().clone();
    assert_eq!(by_key["id"], p["id"]);

    // Validation names the field.
    let bad = |patch: Value| {
        let mut b = gh_body("x", "https://api.github.com");
        for (k, v) in patch.as_object().unwrap() {
            match k.strip_prefix("source.") {
                Some(sk) => b["source"][sk] = v.clone(),
                None => b[k] = v.clone(),
            }
        }
        b
    };
    for (patch, field) in [
        (json!({"source.repo": "nope"}), "source.repo"),
        (
            json!({"source.token_secret": "missing"}),
            "source.token_secret",
        ),
        (json!({"source.api_url": "ftp://x"}), "source.api_url"),
        (json!({"max_runs": 500}), "max_runs"),
        (
            json!({"schedule": {"kind": "interval", "every": "5s"}}),
            "schedule.every",
        ),
    ] {
        let r = td.post(path, bad(patch.clone())).await;
        r.err(422, "invalid");
        assert_eq!(r.body["error"]["details"]["field"], field, "{patch}");
    }
    td.post(path, bad(json!({"source.colour": "red"})))
        .await
        .err(422, "invalid");
    // A script source needs a target that the caller may use.
    let r = td
        .post_as(
            &lead,
            path,
            json!({"name": "octo", "source": {"kind": "script", "target": "nowhere",
                   "script": "true"}}),
        )
        .await;
    r.err(422, "invalid");
    assert_eq!(r.body["error"]["details"]["field"], "source.target");

    // A patch merges into the source; the worker may not patch.
    let pid = id(&p);
    td.patch_as(
        &worker,
        &format!("/api/pipelines/{pid}"),
        json!({"max_runs": 5}),
    )
    .await
    .err(403, "forbidden");
    let u = td
        .patch(
            &format!("/api/pipelines/{pid}"),
            json!({"source": {"branch": "main"}, "max_runs": 5, "description": "the CI"}),
        )
        .await
        .ok()
        .clone();
    assert_eq!(u["source"]["branch"], "main");
    assert_eq!(u["source"]["token_secret"], "gh-token");
    assert_eq!(u["max_runs"], 5);
    // The branch is no new right: run_as stays the lead.
    assert_eq!(u["run_as"], format!("agent:{lead}"));
    let u = td
        .patch(
            &format!("/api/pipelines/{pid}"),
            json!({"source": {"token_secret": "gh-token", "api_url": "https://ghe.example/api/v3"}}),
        )
        .await
        .ok().clone();
    assert_eq!(u["run_as"], "operator");

    // The audit log has the writes, with no secret value.
    let audit = td.get("/api/audit?project=terra").await.ok().clone();
    let text = audit.to_string();
    assert!(text.contains("pipeline.create"), "{text}");
    assert!(text.contains("pipeline.update"), "{text}");
    assert!(!text.contains(GH_TOKEN));

    // Delete.
    let r = td
        .req(
            hyper::Method::DELETE,
            &format!("/api/pipelines/{pid}"),
            None,
            Some(&worker),
        )
        .await;
    r.err(403, "forbidden");
    let r = td.delete(&format!("/api/pipelines/{pid}")).await;
    assert_eq!(r.status, 204, "{}", r.body);
    td.get(&format!("/api/pipelines/{pid}"))
        .await
        .err(404, "not_found");
}

#[tokio::test]
async fn github_polls_runs_jobs_and_events() {
    let td = TestDaemon::start().await;
    let (lead, worker, _) = setup(&td).await;
    let (gh, api) = serve_gh().await;
    gh.answer(
        RUNS,
        200,
        json!({"total_count": 2, "workflow_runs": [
            gh_run(2, "in_progress", None, "2026-11-03T01:00:00Z"),
            gh_run(1, "completed", Some("success"), "2026-11-03T00:00:00Z")]}),
    );
    gh.answer(
        &jobs_path(2),
        200,
        json!({"jobs": [{"name": "build", "status": "completed", "conclusion": "success",
                         "html_url": "https://github.com/o/r/actions/runs/2/job/7"},
                        {"name": "test", "status": "in_progress", "conclusion": null}]}),
    );
    gh.answer(&jobs_path(1), 200, json!({"jobs": []}));
    let p = td
        .post_as(&lead, "/api/projects/terra/pipelines", gh_body("ci", &api))
        .await
        .ok()
        .clone();
    let pid = id(&p);

    // A worker may not refresh; the lead may.
    td.post_empty_as(&worker, &format!("/api/pipelines/{pid}/refresh"))
        .await
        .err(403, "forbidden");
    let r = td
        .post_empty_as(&lead, &format!("/api/pipelines/{pid}/refresh"))
        .await
        .ok()
        .clone();
    assert_eq!(r["poll"]["runs"], 2);
    assert_eq!(r["poll"]["new"], 2);
    assert_eq!(r["pipeline"]["status"], "running");
    assert_eq!(r["pipeline"]["latest"]["id"], "2");
    assert_eq!(r["pipeline"]["latest"]["name"], "CI #2");
    assert_eq!(r["pipeline"]["error"], Value::Null);
    assert_eq!(r["output"], Value::Null);

    // The token went as a bearer token, with the GitHub headers.
    let seen = gh.seen();
    let (q, h) = seen.iter().find(|(q, _)| q.starts_with(RUNS)).unwrap();
    assert!(
        q.contains("per_page=20") && q.contains("exclude_pull_requests=true"),
        "{q}"
    );
    assert_eq!(h["authorization"], format!("Bearer {GH_TOKEN}"));
    assert_eq!(h["x-github-api-version"], "2022-11-28");

    // The runs, newest first; the jobs of both new runs were read.
    let runs = td
        .get_as(&worker, &format!("/api/pipelines/{pid}/runs"))
        .await
        .ok()
        .clone();
    let runs = runs.as_array().unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0]["id"], "2");
    assert_eq!(runs[0]["status"], "running");
    assert_eq!(runs[0]["steps"][1]["name"], "test");
    assert_eq!(runs[0]["steps"][1]["status"], "running");
    assert_eq!(runs[1]["status"], "success");
    assert_eq!(runs[1]["commit"], "sha1");

    // The first poll emits the status, but no run events (the history).
    let st = events(&td, "pipeline.status").await;
    assert_eq!(st.len(), 1);
    assert_eq!(st[0]["data"]["from"], Value::Null);
    assert_eq!(st[0]["data"]["to"], "running");
    assert!(events(&td, "pipeline.run").await.is_empty());

    // Run 2 fails, run 3 is new: two run events and a status event.
    gh.answer(
        RUNS,
        200,
        json!({"workflow_runs": [
            gh_run(3, "queued", None, "2026-11-03T02:00:00Z"),
            gh_run(2, "completed", Some("failure"), "2026-11-03T01:00:00Z"),
            gh_run(1, "completed", Some("success"), "2026-11-03T00:00:00Z")]}),
    );
    let r = td
        .post_empty(&format!("/api/pipelines/{pid}/refresh"))
        .await
        .ok()
        .clone();
    assert_eq!(r["poll"]["new"], 1);
    assert_eq!(r["poll"]["changed"], 1);
    assert_eq!(r["pipeline"]["status"], "queued");
    let ev = events(&td, "pipeline.run").await;
    assert_eq!(ev.len(), 2, "{ev:?}");
    let failed = ev.iter().find(|e| e["data"]["run_id"] == "2").unwrap();
    assert_eq!(failed["data"]["from"], "running");
    assert_eq!(failed["data"]["to"], "failed");
    assert_eq!(failed["data"]["pipeline"], "ci");
    assert_eq!(failed["project"], "terra");
    let st = events(&td, "pipeline.status").await;
    assert_eq!(st.len(), 2);

    // The detail of run 3: no jobs answer yet, so the steps error says
    // why; once GitHub has them, the detail reads and keeps them.
    let d = td
        .get_as(&worker, &format!("/api/pipelines/{pid}/runs/3"))
        .await
        .ok()
        .clone();
    assert_eq!(d["steps"], Value::Null);
    assert!(d["steps_error"].as_str().unwrap().contains("404"), "{d}");
    gh.answer(
        &jobs_path(3),
        200,
        json!({"jobs": [{"name": "build", "status": "queued", "conclusion": null}]}),
    );
    let d = td
        .get(&format!("/api/pipelines/{pid}/runs/3"))
        .await
        .ok()
        .clone();
    assert_eq!(d["steps"][0]["name"], "build");
    assert_eq!(d["adapter"], "github");
    td.get(&format!("/api/pipelines/{pid}/runs/99"))
        .await
        .err(404, "not_found");

    // The rate limit is an error of the poll: one pipeline.error event,
    // the runs stay.
    let reset = (Utc::now() + chrono::Duration::minutes(30)).timestamp();
    gh.answer_with(
        RUNS,
        403,
        vec![
            ("x-ratelimit-remaining", "0".into()),
            ("x-ratelimit-reset", reset.to_string()),
        ],
        json!({"message": "API rate limit exceeded"}),
    );
    for _ in 0..2 {
        let r = td
            .post_empty(&format!("/api/pipelines/{pid}/refresh"))
            .await
            .ok()
            .clone();
        let e = r["pipeline"]["error"].as_str().unwrap();
        assert!(e.contains("rate limit"), "{e}");
        assert_eq!(r["pipeline"]["status"], "queued");
    }
    assert_eq!(events(&td, "pipeline.error").await.len(), 1);
    let runs = td
        .get(&format!("/api/pipelines/{pid}/runs"))
        .await
        .ok()
        .clone();
    assert_eq!(runs.as_array().unwrap().len(), 3);

    // max_runs prunes the old runs.
    gh.answer(
        RUNS,
        200,
        json!({"workflow_runs": [gh_run(3, "queued", None, "2026-11-03T02:00:00Z")]}),
    );
    td.patch(&format!("/api/pipelines/{pid}"), json!({"max_runs": 2}))
        .await
        .ok();
    let runs = td
        .get(&format!("/api/pipelines/{pid}/runs"))
        .await
        .ok()
        .clone();
    let ids: Vec<&str> = runs
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["3", "2"]);
    let r = td
        .post_empty(&format!("/api/pipelines/{pid}/refresh"))
        .await
        .ok()
        .clone();
    assert_eq!(r["pipeline"]["error"], Value::Null);

    // A test poll keeps nothing.
    let t = td
        .post(
            "/api/projects/terra/pipelines/test",
            json!({"source": {"kind": "github", "repo": "o/r", "workflow": "ci.yml",
                              "api_url": api}}),
        )
        .await
        .ok()
        .clone();
    assert_eq!(t["error"], Value::Null, "{t}");
    assert_eq!(t["runs"][0]["id"], "3");
    let l = td.get("/api/projects/terra/pipelines").await.ok().clone();
    assert_eq!(l.as_array().unwrap().len(), 1);
    let none = td
        .post(
            "/api/projects/terra/pipelines/test",
            json!({"source": {"kind": "github", "repo": "o/gone", "api_url": api}}),
        )
        .await
        .ok()
        .clone();
    assert!(none["error"].as_str().unwrap().contains("404"), "{none}");
}

#[tokio::test]
async fn the_tick_polls_due_pipelines() {
    let td = TestDaemon::start().await;
    setup(&td).await;
    let (gh, api) = serve_gh().await;
    gh.answer(
        "/repos/o/r/actions/runs",
        200,
        json!({"workflow_runs": [gh_run(5, "completed", Some("success"), "2026-11-03T00:00:00Z")]}),
    );
    gh.answer(&jobs_path(5), 200, json!({"jobs": []}));
    let mut b = gh_body("all", &api);
    b["source"]["workflow"] = Value::Null;
    b["enabled"] = json!(true);
    b["schedule"] = json!({"kind": "interval", "every": "10m"});
    let p = td
        .post("/api/projects/terra/pipelines", b)
        .await
        .ok()
        .clone();
    let state: Shared = td.daemon.as_ref().unwrap().state.clone();
    let at = hours(2);
    let started = terrariumd::pipelines::tick(&state, at).await;
    assert_eq!(started.len(), 1);
    for h in started {
        h.await.unwrap().unwrap();
    }
    let p = td
        .get(&format!("/api/pipelines/{}", id(&p)))
        .await
        .ok()
        .clone();
    assert_eq!(p["status"], "success");
    assert!(p["polled_at"].is_string());
    // The next poll moved on past the tick.
    let next = DateTime::parse_from_rfc3339(p["next_run_at"].as_str().unwrap()).unwrap();
    assert!(next > at, "{next} is not after {at}");
    // Nothing is due again at the same time.
    assert!(terrariumd::pipelines::tick(&state, at).await.is_empty());
}

// ---------------------------------------------------------- script adapter

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

    async fn host(&self, name: &str) -> tokio::task::JoinHandle<()> {
        let r = self.td.post("/api/hosts", json!({"name": name})).await;
        assert_eq!(r.status, 201, "{}", r.body);
        let token = r.body["token"].as_str().unwrap().to_owned();
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
}

const OCTO_KEY: &str = "API-OCT0PUSK3YF0RT3RRAR1UM";

#[tokio::test]
async fn script_adapter_on_a_host() {
    let lab = Lab::start().await;
    let td = &lab.td;
    let _h = lab.host("wsl").await;
    let (lead, _, _) = setup(td).await;
    let r = td
        .post(
            "/api/targets",
            json!({"name": "wsl", "kind": "host", "host": "wsl"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    td.put(
        "/api/secrets/terra/octo-key",
        json!({"kind": "api_key", "fields": {"value": OCTO_KEY}}),
    )
    .await
    .ok();
    // The script prints the contract JSON; it sees its secret and the
    // pipeline name in its environment.
    let script = r#"cat <<EOF
{"runs": [
  {"id": "Deployments-12", "name": "1.2.3 to Production", "status": "Executing",
   "environment": "Production", "url": "https://octopus.example/app#/d/12",
   "started_at": "2026-11-03T10:00:00+10:00", "summary": "key $OCTOPUS_API_KEY in $TERRARIUM_PIPELINE",
   "steps": [{"name": "Deploy web", "status": "Success"},
             {"name": "Smoke test", "status": "Executing"}]},
  {"id": "Deployments-11", "name": "1.2.2 to Production", "status": "Failed",
   "started_at": "2026-11-02T10:00:00Z"}
]}
EOF"#;
    let body = json!({"name": "octo", "enabled": false,
        "source": {"kind": "script", "target": "wsl", "script": script,
                   "env_secrets": {"OCTOPUS_API_KEY": "octo-key"}}});
    // The lead may not use target `wsl` (not in its policy).
    td.post_as(&lead, "/api/projects/terra/pipelines", body.clone())
        .await
        .err(403, "forbidden");
    // A test poll as the operator: the runs, no row.
    let t = td
        .post("/api/projects/terra/pipelines/test", body.clone())
        .await
        .ok()
        .clone();
    assert_eq!(t["error"], Value::Null, "{t}");
    assert_eq!(t["runs"][0]["status"], "running");
    assert!(
        !t.to_string().contains(OCTO_KEY),
        "the answer hides the secret"
    );
    let p = td
        .post("/api/projects/terra/pipelines", body.clone())
        .await
        .ok()
        .clone();
    let pid = id(&p);
    let r = td
        .post_empty(&format!("/api/pipelines/{pid}/refresh"))
        .await
        .ok()
        .clone();
    assert_eq!(r["pipeline"]["status"], "running", "{r}");
    assert_eq!(r["output"]["exit_code"], 0);
    assert!(!r.to_string().contains(OCTO_KEY), "{r}");
    let runs = td
        .get(&format!("/api/pipelines/{pid}/runs"))
        .await
        .ok()
        .clone();
    let runs = runs.as_array().unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0]["id"], "Deployments-12");
    assert_eq!(runs[0]["environment"], "Production");
    assert_eq!(runs[0]["started_at"], "2026-11-03T00:00:00.000Z");
    assert_eq!(runs[0]["steps"][0]["status"], "success");
    let summary = runs[0]["summary"].as_str().unwrap();
    assert!(!summary.contains(OCTO_KEY), "{summary}");
    assert!(summary.ends_with("in octo"), "{summary}");
    assert_eq!(runs[1]["status"], "failed");
    // The detail of a script run has the steps it gave.
    let d = td
        .get(&format!("/api/pipelines/{pid}/runs/Deployments-11"))
        .await
        .ok()
        .clone();
    assert_eq!(d["steps"], Value::Null);
    assert!(d.get("steps_error").is_none());

    // Bad JSON and an exit code that is not 0 are poll errors.
    td.patch(
        &format!("/api/pipelines/{pid}"),
        json!({"source": {"script": "echo '{\"runs\": [{\"id\": 1, \"status\": \"meh\"}]}'"}}),
    )
    .await
    .ok();
    let r = td
        .post_empty(&format!("/api/pipelines/{pid}/refresh"))
        .await
        .ok()
        .clone();
    let e = r["pipeline"]["error"].as_str().unwrap();
    assert!(e.contains("runs[0].status"), "{e}");
    td.patch(
        &format!("/api/pipelines/{pid}"),
        json!({"source": {"script": "echo 'no login' >&2; exit 3"}}),
    )
    .await
    .ok();
    let r = td
        .post_empty(&format!("/api/pipelines/{pid}/refresh"))
        .await
        .ok()
        .clone();
    let e = r["pipeline"]["error"].as_str().unwrap();
    assert_eq!(e, "the script exited with code 3: no login");
    assert_eq!(r["output"]["stderr"], "no login\n");
    assert_eq!(events(td, "pipeline.error").await.len(), 1);
}
