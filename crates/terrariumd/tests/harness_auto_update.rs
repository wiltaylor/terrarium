//! The harness auto-update (`terrariumd::harness_auto_update`) and the
//! pins: `POST /api/harnesses/check`, `POST /api/harnesses/{name}/pin`,
//! and `.../unpin`, with a fake npm registry (an `HttpGet` that answers
//! from a table) and a fake npm program (`harness_tools::FAKE_NPM`). No
//! network.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use common::TestDaemon;
use serde_json::{Value, json};
use terrariumd::harness_tools::{ToolsConfig, compiled_pins, write_fake_npm};
use terrariumd::probes::{HttpGet, HttpRequest, HttpResponse};

/// The npm registry: `dist-tags.latest` of each package; a package with no
/// entry answers 503.
#[derive(Default)]
struct Registry {
    latest: Mutex<HashMap<String, String>>,
}

impl Registry {
    fn set(&self, package: &str, version: &str) {
        self.latest
            .lock()
            .unwrap()
            .insert(package.to_owned(), version.to_owned());
    }
}

impl HttpGet for Registry {
    fn get(
        &self,
        req: HttpRequest,
    ) -> futures_util::future::BoxFuture<'_, Result<HttpResponse, String>> {
        let found = self
            .latest
            .lock()
            .unwrap()
            .iter()
            .find(|(p, _)| req.url.ends_with(&p.replace('/', "%2f")))
            .map(|(_, v)| v.clone());
        Box::pin(async move {
            Ok(match found {
                Some(v) => HttpResponse {
                    status: 200,
                    body: json!({"dist-tags": {"latest": v}}).to_string(),
                },
                None => HttpResponse {
                    status: 503,
                    body: "down".into(),
                },
            })
        })
    }
}

/// A daemon with a tools dir that holds every pin, and a registry where
/// claude and copilot are at their pins, codex is at `codex`, and pi does
/// not answer.
async fn daemon(codex: &str) -> (TestDaemon, ToolsConfig, Arc<Registry>) {
    let td = TestDaemon::start().await;
    let root = td.dir.path().join("tools-root");
    std::fs::create_dir_all(root.join("copilot")).unwrap();
    let mut cfg = ToolsConfig::new(root.join("tools"), compiled_pins(), root.join("copilot"));
    cfg.npm = write_fake_npm(&root);
    let r = cfg.install().await;
    assert_eq!(r.failed(), 0, "{r:#?}");
    let state = &td.daemon.as_ref().unwrap().state;
    state.harness_tools.configure(cfg.clone());
    let reg = Arc::new(Registry::default());
    for p in cfg.pins.iter().filter(|p| !p.extension) {
        if p.name != "pi" {
            reg.set(&p.package, &p.version);
        }
    }
    reg.set("@openai/codex", codex);
    state.harness_latest.set_http(reg.clone());
    (td, cfg, reg)
}

fn pin(cfg: &ToolsConfig, name: &str) -> String {
    cfg.pin(name).unwrap().version.clone()
}

/// The open operator mail: (subject, body).
async fn operator_mail(td: &TestDaemon) -> Vec<(String, String)> {
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .read(|c| terrariumd::store::mail::operator_mail(c, true, 100))
        .await
        .unwrap()
        .into_iter()
        .map(|m| (m.subject, m.body))
        .collect()
}

fn row<'a>(results: &'a Value, harness: &str) -> &'a Value {
    results
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["harness"] == harness)
        .unwrap_or_else(|| panic!("no {harness} in {results}"))
}

async fn harness_row(td: &TestDaemon, harness: &str) -> Value {
    let v = td.get("/api/harnesses").await;
    row(&v.ok()["harnesses"], harness).clone()
}

#[tokio::test]
async fn a_check_installs_a_newer_release_once_and_mails_once() {
    let (td, cfg, _reg) = daemon("0.165.0").await;
    let codex_pin = pin(&cfg, "codex");

    let r = td.post_empty("/api/harnesses/check").await;
    let b = r.ok().clone();
    let codex = row(&b["results"], "codex");
    assert_eq!(codex["result"], "updated");
    assert_eq!(codex["from"], codex_pin.as_str());
    assert_eq!(codex["to"], "0.165.0");
    assert_eq!(codex["in_range"], false, "the pin range ends under 0.165.0");
    assert_eq!(codex["mailed"], true);
    assert_eq!(row(&b["results"], "claude")["result"], "current");
    assert_eq!(row(&b["results"], "copilot")["result"], "current");
    let pi = row(&b["results"], "pi");
    assert_eq!(pi["result"], "skipped");
    assert!(pi["detail"].as_str().unwrap().contains("503"), "{pi}");
    assert!(
        b["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["harness"] != "copilot-prompt"),
        "the extension has no policy"
    );
    assert_eq!(cfg.read_manifest()["codex"].version, "0.165.0");

    // One mail, with the warning and the rollback line.
    let mail = operator_mail(&td).await;
    assert_eq!(mail.len(), 1, "{mail:?}");
    let (subject, body) = &mail[0];
    assert_eq!(
        subject,
        &format!("Harness update: codex {codex_pin} -> 0.165.0 (outside the tested range)")
    );
    assert!(
        body.contains("WARNING: 0.165.0 is outside the tested range"),
        "{body}"
    );
    assert!(
        body.contains(&format!("terrarium admin harness pin codex {codex_pin}")),
        "{body}"
    );
    assert!(body.contains("/settings/subscriptions"), "{body}");

    // The audit row and the event.
    let audit = td.get("/api/audit?action=harness.auto_update").await;
    let rows = audit.ok().as_array().unwrap().clone();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["actor"], "system");
    assert_eq!(rows[0]["detail"]["ok"], true);
    let ev = td.get("/api/events?kind=harness").await;
    let ev = ev.ok().as_array().unwrap().clone();
    assert_eq!(ev.last().unwrap()["data"]["change"], "auto_updated");

    // Nothing changed: no install and no mail.
    let again = td.post_empty("/api/harnesses/check").await;
    let codex = row(&again.ok()["results"], "codex").clone();
    assert_eq!(codex["result"], "current");
    assert_eq!(codex["mailed"], false);
    assert_eq!(operator_mail(&td).await.len(), 1);

    // The table: the policy, the last check, and the previous version
    // (kept on disk).
    let h = harness_row(&td, "codex").await;
    assert_eq!(h["active"], "0.165.0");
    assert_eq!(h["versions"], json!([codex_pin, "0.165.0"]));
    let au = &h["auto_update"];
    assert_eq!(au["policy"], "auto");
    assert_eq!(au["pinned_version"], Value::Null);
    assert_eq!(au["previous"], codex_pin.as_str());
    assert_eq!(au["latest"], "0.165.0");
    assert_eq!(au["result"], "current");
    assert!(au["checked_at"].is_string());
    assert_eq!(au["changelog"], "https://github.com/openai/codex/releases");
    let report = td.get("/api/harnesses").await;
    assert_eq!(
        report.ok()["auto_update"],
        json!({"schedule": "off", "next_check_at": null}),
        "no background task in the tests"
    );
}

#[tokio::test]
async fn a_failed_install_keeps_the_version_and_mails_once_for_each_version() {
    let (td, cfg, reg) = daemon("99.0.0").await;
    let codex_pin = pin(&cfg, "codex");
    for mailed in [true, false] {
        let r = td.post_empty("/api/harnesses/check").await;
        let codex = row(&r.ok()["results"], "codex").clone();
        assert_eq!(codex["result"], "failed");
        assert_eq!(codex["mailed"], mailed, "{codex}");
        assert_eq!(cfg.read_manifest()["codex"].version, codex_pin);
    }
    let mail = operator_mail(&td).await;
    assert_eq!(mail.len(), 1, "{mail:?}");
    assert_eq!(mail[0].0, "Harness update failed: codex 99.0.0");
    assert!(
        mail[0].1.contains(&format!("codex stays at {codex_pin}")),
        "{}",
        mail[0].1
    );
    let audit = td.get("/api/audit?action=harness.auto_update").await;
    let rows = audit.ok().as_array().unwrap().clone();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r["detail"]["ok"] == false));

    // A new failed version mails again.
    reg.set("@openai/codex", "99.0.1");
    let r = td.post_empty("/api/harnesses/check").await;
    assert_eq!(row(&r.ok()["results"], "codex")["mailed"], true);
    assert_eq!(operator_mail(&td).await.len(), 2);
    assert_eq!(
        harness_row(&td, "codex").await["auto_update"]["result"],
        "failed"
    );
}

#[tokio::test]
async fn rollback_and_pin_then_unpin() {
    let (td, cfg, _reg) = daemon("0.165.0").await;
    let codex_pin = pin(&cfg, "codex");
    td.post_empty("/api/harnesses/check").await.ok();
    assert_eq!(cfg.read_manifest()["codex"].version, "0.165.0");

    // Roll back: the pin with no version goes to the previous version at
    // once, with no npm run.
    let r = td.post_empty("/api/harnesses/codex/pin").await;
    let b = r.ok().clone();
    assert_eq!(b["policy"], "pinned");
    assert_eq!(b["pinned_version"], codex_pin.as_str());
    assert_eq!(b["from"], "0.165.0");
    assert_eq!(b["to"], codex_pin.as_str());
    assert!(b.get("output").is_none(), "no npm run: {b}");
    assert_eq!(cfg.read_manifest()["codex"].version, codex_pin);
    // The newer version stays on disk: it is the previous version now.
    assert_eq!(cfg.versions("codex"), [codex_pin.as_str(), "0.165.0"]);
    let h = harness_row(&td, "codex").await;
    assert_eq!(h["auto_update"]["policy"], "pinned");
    assert_eq!(h["auto_update"]["pinned_version"], codex_pin.as_str());
    assert_eq!(h["auto_update"]["previous"], "0.165.0");

    // A pinned harness is not updated, and gets no mail.
    let mails = operator_mail(&td).await.len();
    let r = td.post_empty("/api/harnesses/check").await;
    let codex = row(&r.ok()["results"], "codex").clone();
    assert_eq!(codex["result"], "pinned");
    assert_eq!(codex["latest"], "0.165.0");
    assert_eq!(cfg.read_manifest()["codex"].version, codex_pin);
    assert_eq!(operator_mail(&td).await.len(), mails);

    // A manual update of a pinned harness moves the pin.
    let r = td
        .post("/api/harnesses/codex/update", json!({"version": "0.164.5"}))
        .await;
    assert_eq!(r.ok()["to"], "0.164.5");
    let h = harness_row(&td, "codex").await;
    assert_eq!(h["auto_update"]["pinned_version"], "0.164.5");
    assert_eq!(h["auto_update"]["previous"], codex_pin.as_str());

    // Pin to a version that is not on disk: npm installs it.
    let r = td
        .post("/api/harnesses/codex/pin", json!({"version": "0.170.0"}))
        .await;
    let b = r.ok().clone();
    assert_eq!(b["to"], "0.170.0");
    assert!(
        b["output"].as_str().unwrap().contains("added 1 package"),
        "{b}"
    );
    // "Pin current": the active version, no switch.
    let r = td
        .post("/api/harnesses/codex/pin", json!({"version": "0.170.0"}))
        .await;
    assert_eq!(r.ok()["from"], "0.170.0");

    // Unpin: back to auto; the active version stays until the next check.
    let r = td.post_empty("/api/harnesses/codex/unpin").await;
    let b = r.ok().clone();
    assert_eq!(b["policy"], "auto");
    assert_eq!(b["pinned_version"], Value::Null);
    assert_eq!(b["to"], "0.170.0");
    assert_eq!(cfg.read_manifest()["codex"].version, "0.170.0");
    let h = harness_row(&td, "codex").await;
    assert_eq!(h["auto_update"]["policy"], "auto");

    let audit = |action: &'static str| {
        let td = &td;
        async move {
            td.get(&format!("/api/audit?action={action}"))
                .await
                .ok()
                .as_array()
                .unwrap()
                .len()
        }
    };
    assert_eq!(audit("harness.pin").await, 3);
    assert_eq!(audit("harness.unpin").await, 1);
}

#[tokio::test]
async fn a_pin_needs_a_previous_version_or_a_good_version() {
    let (td, _cfg, _reg) = daemon("0.165.0").await;
    let r = td.post_empty("/api/harnesses/pi/pin").await;
    r.err(422, "invalid");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no previous version"),
        "{}",
        r.body
    );
    for v in ["latest", "1.2", "--global"] {
        td.post("/api/harnesses/pi/pin", json!({"version": v}))
            .await
            .err(422, "invalid");
    }
    td.post("/api/harnesses/pi/pin", json!({"versoin": "1.0.0"}))
        .await
        .err(400, "bad_request");
    td.post_empty("/api/harnesses/copilot-prompt/pin")
        .await
        .err(422, "invalid");
    // An npm failure: 422 with the output, and the policy stays.
    let r = td
        .post("/api/harnesses/pi/pin", json!({"version": "99.0.0"}))
        .await;
    r.err(422, "invalid");
    assert!(
        r.body["error"]["details"]["output"]
            .as_str()
            .unwrap()
            .contains("ETARGET")
    );
    assert_eq!(
        harness_row(&td, "pi").await["auto_update"]["policy"],
        "auto"
    );
}

/// The check skips a harness while a manual update holds the lock, and
/// records the time of the clock it is given.
#[tokio::test]
async fn a_manual_update_makes_the_check_skip_and_the_clock_is_recorded() {
    let (td, cfg, _reg) = daemon("0.165.0").await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    let now = chrono::DateTime::parse_from_rfc3339("2030-01-02T03:04:05Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let busy = state.harness_tools.busy.lock().await;
    let r = terrariumd::harness_auto_update::run_check(&state, now).await;
    drop(busy);
    let codex = r.results.iter().find(|i| i.harness == "codex").unwrap();
    assert_eq!(codex.result.as_str(), "skipped");
    assert!(codex.detail.contains("manual"), "{}", codex.detail);
    assert_eq!(cfg.read_manifest()["codex"].version, pin(&cfg, "codex"));
    assert_eq!(r.checked_at, now);
    let h = harness_row(&td, "codex").await;
    assert_eq!(h["auto_update"]["checked_at"], "2030-01-02T03:04:05.000Z");
    assert!(operator_mail(&td).await.is_empty());
}

#[tokio::test]
async fn only_the_operator_or_a_sysadmin_pins_and_checks() {
    let (td, _cfg, _reg) = daemon("0.165.0").await;
    td.project("terra").await;
    let plain = td.agent("terra", "plain", json!({})).await;
    let plain = plain["id"].as_str().unwrap();
    for path in [
        "/api/harnesses/check",
        "/api/harnesses/codex/pin",
        "/api/harnesses/codex/unpin",
    ] {
        td.post_empty_as(plain, path).await.err(403, "forbidden");
    }
    let root = td
        .agent("terra", "root", json!({"roles": ["sysadmin"]}))
        .await;
    let root = root["id"].as_str().unwrap();
    td.post_empty_as(root, "/api/harnesses/codex/unpin")
        .await
        .ok();
    // Every caller reads the table.
    let h = td.get_as(plain, "/api/harnesses").await;
    assert!(h.ok()["harnesses"][0]["auto_update"].is_object());

    let bare = TestDaemon::start().await;
    for path in ["/api/harnesses/check", "/api/harnesses/codex/pin"] {
        bare.post_empty(path).await.err(501, "not_available");
    }
}
