//! `POST /api/harnesses/{name}/update` and `POST /api/harnesses/update`:
//! the update into a version dir of the tools dir and the switch to it,
//! with a fake npm (`terrariumd::harness_tools::FAKE_NPM`). The update with
//! a real runner and a running agent is in `runner_process.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::TestDaemon;
use serde_json::{Value, json};
use terrariumd::harness_tools::{ToolsConfig, compiled_pins, write_fake_npm};

/// A daemon with a tools dir that holds every pin (installed by the fake
/// npm), and the npm log.
async fn daemon() -> (TestDaemon, ToolsConfig) {
    let td = TestDaemon::start().await;
    let root = td.dir.path().join("tools-root");
    std::fs::create_dir_all(root.join("copilot")).unwrap();
    let mut cfg = ToolsConfig::new(root.join("tools"), compiled_pins(), root.join("copilot"));
    cfg.npm = write_fake_npm(&root);
    let r = cfg.install().await;
    assert_eq!(r.failed(), 0, "{r:#?}");
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .harness_tools
        .configure(cfg.clone());
    (td, cfg)
}

fn npm_runs(cfg: &ToolsConfig) -> Vec<String> {
    std::fs::read_to_string(cfg.dir.parent().unwrap().join("npm.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn an_update_installs_a_version_dir_and_answers_from_and_to() {
    let (td, cfg) = daemon().await;
    let pin = cfg.pin("codex").unwrap().version.clone();
    let runs = npm_runs(&cfg).len();

    // The same version: its dir is complete, so npm does not run.
    let r = td
        .post("/api/harnesses/codex/update", json!({"version": pin}))
        .await;
    let b = r.ok();
    assert_eq!(b["harness"], "codex");
    assert_eq!(b["package"], "@openai/codex");
    assert_eq!(b["from"], pin.as_str());
    assert_eq!(b["to"], pin.as_str());
    assert_eq!(b["reused"], true);
    assert_eq!(b["agents_on_old_version"], json!([]));
    assert_eq!(npm_runs(&cfg).len(), runs);
    // `force` installs it again.
    let r = td
        .post(
            "/api/harnesses/codex/update",
            json!({"version": pin, "force": true}),
        )
        .await;
    let b = r.ok();
    assert_eq!(b["reused"], false);
    assert!(
        b["output"].as_str().unwrap().contains("added 1 package"),
        "{b}"
    );
    assert!(
        npm_runs(&cfg)
            .last()
            .unwrap()
            .ends_with(&format!("@openai/codex@{pin}"))
    );

    // An empty body is `latest`. No agent runs the old version, but it is
    // the previous version (the rollback target), so the GC keeps it.
    let r = td.post_empty("/api/harnesses/codex/update").await;
    let b = r.ok();
    assert_eq!(b["to"], "9.0.0");
    assert!(b.get("removed").is_none(), "{b}");
    assert_eq!(cfg.read_manifest()["codex"].version, "9.0.0");
    assert_eq!(cfg.versions("codex"), [pin.as_str(), "9.0.0"]);

    // The harness table shows the active version, the dirs on disk, and
    // the previous version.
    let h = td.get("/api/harnesses").await;
    let rows = h.ok()["harnesses"].as_array().unwrap().clone();
    let codex = rows.iter().find(|r| r["harness"] == "codex").unwrap();
    assert_eq!(codex["active"], "9.0.0");
    assert_eq!(codex["versions"], json!([pin, "9.0.0"]));
    assert_eq!(codex["auto_update"]["policy"], "auto");
    assert_eq!(codex["auto_update"]["previous"], pin.as_str());

    // The audit row and the `harness` event.
    let audit = td.get("/api/audit?action=harness.update").await;
    let rows = audit.ok().as_array().unwrap().clone();
    assert_eq!(rows.len(), 3, "{rows:?}");
    let ev = td.get("/api/events?kind=harness").await;
    let ev = ev.ok().as_array().unwrap().clone();
    let last: &Value = ev.last().unwrap();
    assert_eq!(last["data"]["change"], "updated");
    assert_eq!(last["data"]["from"], pin.as_str());
    assert_eq!(last["data"]["to"], "9.0.0");
    assert_eq!(last["data"]["forced"], false);
}

/// A running agent does not stop an update: the answer lists it, with the
/// note that it moves to the new version at its next restart. The version
/// it may run stays on disk. (This supervisor reports no versions, so the
/// version of the agent is not known.)
#[tokio::test]
async fn a_running_agent_does_not_stop_the_update() {
    let (td, cfg) = daemon().await;
    td.project("terra").await;
    let a = td
        .agent("terra", "worker", json!({"harness": "codex"}))
        .await;
    let id = a["id"].as_str().unwrap().to_owned();
    td.post_empty(&format!("/api/agents/{id}/start")).await.ok();
    td.wait_status(&id, "idle").await;

    let r = td.post("/api/harnesses/codex/update", json!({})).await;
    let b = r.ok();
    assert_eq!(b["to"], "9.0.0");
    assert_eq!(
        b["agents_on_old_version"],
        json!([{"agent": "terra/worker", "id": id, "version": null}])
    );
    let note = b["note"].as_str().unwrap();
    assert!(
        note.contains("moves to 9.0.0 at its next restart"),
        "{note}"
    );
    // Another harness has no agent on it.
    let p = td.post("/api/harnesses/pi/update", json!({})).await;
    assert_eq!(p.ok()["agents_on_old_version"], json!([]));
    // `update all` goes ahead too.
    let all = td.post("/api/harnesses/update", json!({})).await;
    assert_eq!(
        all.ok()["results"].as_array().unwrap().len(),
        cfg.pins.len()
    );
    let ev = td.get("/api/events?kind=harness").await;
    let ev = ev.ok().as_array().unwrap().clone();
    let codex = ev.iter().find(|e| e["data"]["harness"] == "codex").unwrap();
    assert_eq!(
        codex["data"]["agents_on_old_version"],
        json!(["terra/worker"])
    );
}

#[tokio::test]
async fn a_bad_version_or_an_npm_error_is_invalid_with_the_output() {
    let (td, cfg) = daemon().await;
    let runs = npm_runs(&cfg).len();
    for body in [json!({"version": "--global"}), json!({"version": "1.2"})] {
        td.post("/api/harnesses/codex/update", body)
            .await
            .err(422, "invalid");
    }
    td.post("/api/harnesses/nope/update", json!({}))
        .await
        .err(422, "invalid");
    td.post("/api/harnesses/codex/update", json!({"versoin": "1.0.0"}))
        .await
        .err(400, "bad_request");
    assert_eq!(npm_runs(&cfg).len(), runs, "nothing reached npm");

    let before = cfg.read_manifest();
    let r = td
        .post("/api/harnesses/codex/update", json!({"version": "99.0.0"}))
        .await;
    r.err(422, "invalid");
    let d = &r.body["error"]["details"];
    assert!(d["output"].as_str().unwrap().contains("ETARGET"), "{d}");
    assert_eq!(d["package"], "@openai/codex");
    assert_eq!(d["version"], "99.0.0");
    assert_eq!(d["from"], cfg.pin("codex").unwrap().version.as_str());
    assert_eq!(cfg.read_manifest(), before);
    // The failure has an audit row, but no event.
    let audit = td.get("/api/audit?action=harness.update").await;
    let rows = audit.ok().as_array().unwrap().clone();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["detail"]["ok"], false, "{rows:?}");
    assert!(
        td.get("/api/events?kind=harness")
            .await
            .ok()
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn update_all_takes_every_pin_to_the_latest() {
    let (td, cfg) = daemon().await;
    let r = td.post_empty("/api/harnesses/update").await;
    let results = r.ok()["results"].as_array().unwrap().clone();
    assert_eq!(results.len(), cfg.pins.len());
    assert!(results.iter().all(|x| x["to"] == "9.0.0"), "{results:?}");
    let names: Vec<&str> = results
        .iter()
        .map(|x| x["harness"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"copilot-prompt"), "{names:?}");
    td.post("/api/harnesses/update", json!({"version": "1.0.0"}))
        .await
        .err(422, "invalid");
}

#[tokio::test]
async fn only_the_operator_or_a_sysadmin_updates_and_a_daemon_without_tools_says_so() {
    let (td, _cfg) = daemon().await;
    td.project("terra").await;
    let a = td.agent("terra", "plain", json!({})).await;
    let id = a["id"].as_str().unwrap();
    td.post_as(id, "/api/harnesses/codex/update", json!({}))
        .await
        .err(403, "forbidden");

    let bare = TestDaemon::start().await;
    bare.post("/api/harnesses/codex/update", json!({}))
        .await
        .err(501, "not_available");
}
