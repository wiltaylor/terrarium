//! Test runs: upload in each body form, list filters, the run page with
//! paging, the original file, compare, links and their rights, delete,
//! events, and audit (PLAN §15.5, docs/api.md §29).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use base64::Engine;
use common::TestDaemon;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::header::HeaderMap;
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::UnixStream;

const NEXTEST: &str = include_str!("../src/test_results/fixtures/nextest-junit.xml");
const CTRF: &str = include_str!("../src/test_results/fixtures/ctrf.json");
const TRX: &str = include_str!("../src/test_results/fixtures/dotnet.trx");
const TAP: &str = include_str!("../src/test_results/fixtures/node.tap");

struct Raw {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Raw {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

async fn raw(
    td: &TestDaemon,
    method: Method,
    path: &str,
    content_type: Option<&str>,
    body: Vec<u8>,
    agent: Option<&str>,
) -> Raw {
    let stream = UnixStream::connect(&td.socket).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost");
    if let Some(ct) = content_type {
        req = req.header("content-type", ct);
    }
    if let Some(a) = agent {
        req = req.header("x-terrarium-agent", a);
    }
    let resp = sender
        .send_request(req.body(Full::new(Bytes::from(body))).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    Raw {
        status,
        headers,
        body,
    }
}

async fn setup() -> TestDaemon {
    let td = TestDaemon::start().await;
    td.post(
        "/api/projects",
        json!({"name": "proj", "card_prefix": "TER"}),
    )
    .await
    .ok();
    td.agent("proj", "lead", json!({"roles": ["project_admin"]}))
        .await;
    td.agent("proj", "w1", json!({})).await;
    td.agent("proj", "w2", json!({})).await;
    td.project("other").await;
    td.agent("other", "x", json!({})).await;
    td
}

async fn card(td: &TestDaemon, who: &str, title: &str) -> String {
    td.post_as(who, "/api/projects/proj/cards", json!({"title": title}))
        .await
        .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn push(td: &TestDaemon, who: &str, body: Value) -> Value {
    td.post_as(who, "/api/projects/proj/test-runs", body)
        .await
        .ok()
        .clone()
}

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

/// A JUnit file of one suite: `(name, status)` for each case.
fn junit(cases: &[(&str, &str)]) -> String {
    let mut x = String::from("<testsuites><testsuite name=\"s\">");
    for (n, s) in cases {
        match *s {
            "failed" => x.push_str(&format!(
                "<testcase name=\"{n}\" classname=\"c\"><failure message=\"{n} broke\"/></testcase>"
            )),
            "skipped" => x.push_str(&format!(
                "<testcase name=\"{n}\" classname=\"c\"><skipped/></testcase>"
            )),
            _ => x.push_str(&format!("<testcase name=\"{n}\" classname=\"c\"/>")),
        }
    }
    x.push_str("</testsuite></testsuites>");
    x
}

#[tokio::test]
async fn upload_list_show_raw_and_the_card_chip() {
    let td = setup().await;
    let c1 = card(&td, "proj/w1", "Parser").await;
    let run = push(
        &td,
        "proj/w1",
        json!({"content": NEXTEST, "file_name": "junit.xml", "cards": [c1],
               "labels": ["version:1.0.0"], "commit": "4be1c0d", "branch": "main"}),
    )
    .await;
    assert_eq!(run["format"], "junit");
    assert_eq!(run["tool"], "cargo-nextest");
    assert_eq!(run["name"], "cargo-nextest");
    assert_eq!(run["status"], "failed");
    assert_eq!(
        run["counts"],
        json!({"passed": 3, "failed": 1, "errored": 1, "skipped": 1, "total": 6})
    );
    assert_eq!(run["duration_ms"], 3207);
    assert_eq!(run["commit"], "4be1c0d");
    assert_eq!(run["branch"], "main");
    assert_eq!(run["file_name"], "junit.xml");
    assert_eq!(run["raw_size"], NEXTEST.len());
    assert_eq!(run["cards"], json!([c1]));
    assert_eq!(run["labels"], json!(["version:1.0.0"]));
    assert_eq!(run["created_by_name"], "proj/w1");
    // Legacy version fields are unknown JSON fields.
    let r = td
        .post_as(
            "proj/w1",
            "/api/projects/proj/test-runs",
            json!({"content": "ok 1", "versions": ["v1"]}),
        )
        .await;
    r.err(400, "bad_request");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown field"),
        "{}",
        r.body
    );
    let rid = id(&run);
    assert_eq!(run["url"], format!("/p/proj/tests/{rid}"));

    // Board summaries keep only the compact result chip, identical to detail.
    let b = td.get("/api/projects/proj/board").await;
    let cards = b.ok()["cards"].as_array().unwrap().clone();
    let summary_chip = &cards.iter().find(|c| c["id"] == c1.as_str()).unwrap()["tests"];
    let full = td.get(&format!("/api/cards/{c1}")).await;
    let chip = &full.ok()["tests"];
    assert_eq!(summary_chip, chip);
    assert_eq!(summary_chip.as_object().unwrap().len(), 5);
    assert!(summary_chip.get("cases").is_none());
    assert!(summary_chip.get("suites").is_none());
    assert_eq!(chip["id"], rid.as_str());
    assert_eq!(chip["status"], "failed");
    assert_eq!(chip["counts"]["failed"], 1);

    // The run page: every suite, a page of cases, and a filter.
    let d = td.get(&format!("/api/test-runs/{rid}")).await;
    let d = d.ok();
    assert_eq!(d["suites"].as_array().unwrap().len(), 2);
    assert_eq!(d["suites"][0]["name"], "calc");
    assert_eq!(d["suites"][0]["counts"]["total"], 4);
    assert_eq!(d["cases"].as_array().unwrap().len(), 6);
    assert_eq!(d["cases_total"], 6);
    assert_eq!(d["cases"][0]["seq"], 0);
    assert_eq!(d["next_cursor"], Value::Null);
    let f = td.get(&format!("/api/test-runs/{rid}?status=failed")).await;
    let f = f.ok();
    assert_eq!(f["cases_total"], 2);
    let names: Vec<&str> = f["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["tests::adds_negative_numbers", "survives_a_restart"]
    );
    assert_eq!(f["cases"][0]["suite"], "calc");
    assert!(
        f["cases"][0]["trace"]
            .as_str()
            .unwrap()
            .contains("right: -3")
    );
    let p1 = td.get(&format!("/api/test-runs/{rid}?limit=4")).await;
    let p1 = p1.ok();
    assert_eq!(p1["cases"].as_array().unwrap().len(), 4);
    assert_eq!(p1["next_cursor"], "3");
    let p2 = td
        .get(&format!("/api/test-runs/{rid}?limit=4&cursor=3"))
        .await;
    assert_eq!(p2.ok()["cases"].as_array().unwrap().len(), 2);
    let q = td.get(&format!("/api/test-runs/{rid}?q=DIVISION")).await;
    assert_eq!(q.ok()["cases_total"], 1);
    let q = td
        .get(&format!(
            "/api/test-runs/{rid}?q=integration&status=errored"
        ))
        .await;
    assert_eq!(q.ok()["cases_total"], 1);
    td.get(&format!("/api/test-runs/{rid}?status=odd"))
        .await
        .err(400, "bad_request");

    // The original file.
    let r = raw(
        &td,
        Method::GET,
        &format!("/api/test-runs/{rid}/raw"),
        None,
        vec![],
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body, NEXTEST.as_bytes());
    assert_eq!(r.headers["content-type"], "application/xml");
    assert_eq!(r.headers["x-content-type-options"], "nosniff");
    assert!(
        r.headers["content-disposition"]
            .to_str()
            .unwrap()
            .starts_with("attachment; filename=\"junit.xml\"")
    );

    // More runs, then the list filters.
    let ok = junit(&[("a", "passed")]);
    let r2 = push(&td, "proj/w2", json!({"content": ok, "name": "smoke"})).await;
    let r3 = push(&td, "proj/lead", json!({"content": CTRF, "name": "e2e"})).await;
    assert_eq!(r2["status"], "passed");
    assert_eq!(r3["tool"], "playwright");
    let all = td.get("/api/projects/proj/test-runs").await;
    let ids: Vec<String> = all.ok().as_array().unwrap().iter().map(id).collect();
    assert_eq!(ids, [id(&r3), id(&r2), rid.clone()]);
    let page = td.get("/api/projects/proj/test-runs?limit=2").await;
    let cursor = page.next_cursor().unwrap();
    let rest = td
        .get(&format!(
            "/api/projects/proj/test-runs?limit=2&cursor={cursor}"
        ))
        .await;
    assert_eq!(id(&rest.ok()[0]), rid);
    let by_card = td
        .get(&format!("/api/projects/proj/test-runs?card={c1}"))
        .await;
    assert_eq!(by_card.ok().as_array().unwrap().len(), 1);
    let by_card = td.get("/api/projects/proj/test-runs?card=ter-1").await;
    assert_eq!(by_card.ok().as_array().unwrap().len(), 1);
    let failed = td.get("/api/projects/proj/test-runs?status=failed").await;
    assert_eq!(failed.ok().as_array().unwrap().len(), 2);
    let passed = td.get("/api/projects/proj/test-runs?status=passed").await;
    assert_eq!(passed.ok().as_array().unwrap().len(), 1);
    let named = td.get("/api/projects/proj/test-runs?name=e2e").await;
    assert_eq!(named.ok().as_array().unwrap().len(), 1);
    td.get("/api/projects/proj/test-runs?status=green")
        .await
        .err(400, "bad_request");

    // Events: one `test_run` event for each upload.
    let ev = td.get("/api/events?kind=test_run").await;
    let ev = ev.ok().as_array().unwrap().clone();
    assert_eq!(ev.len(), 3);
    let first = ev.iter().find(|e| e["data"]["id"] == rid.as_str()).unwrap();
    assert_eq!(first["data"]["change"], "created");
    assert_eq!(first["data"]["cards"], json!([c1]));
    assert_eq!(first["data"]["counts"]["failed"], 1);
    assert_eq!(first["project"], "proj");
    assert_eq!(first["agent"], "w1");
}

#[tokio::test]
async fn upload_body_forms_and_errors() {
    let td = setup().await;
    // The file itself, with the fields in the query.
    let r = raw(
        &td,
        Method::POST,
        "/api/projects/proj/test-runs?name=dotnet&branch=dev&file_name=results.trx",
        Some("application/xml"),
        TRX.as_bytes().to_vec(),
        Some("proj/w1"),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.json());
    let v = r.json();
    assert_eq!(v["format"], "trx");
    assert_eq!(v["tool"], "xunit");
    assert_eq!(v["name"], "dotnet");
    assert_eq!(v["branch"], "dev");
    // A JSON report as the body: not the envelope.
    let r = raw(
        &td,
        Method::POST,
        "/api/projects/proj/test-runs",
        Some("application/json"),
        CTRF.as_bytes().to_vec(),
        Some("proj/w1"),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED);
    assert_eq!(r.json()["format"], "ctrf");
    assert_eq!(r.json()["name"], "shop-web");
    assert_eq!(r.json()["file_name"], "ctrf.json");
    // Multipart, with the name from the file.
    let mut body = Vec::new();
    body.extend_from_slice(
        b"--XyZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"node.tap\"\r\nContent-Type: text/plain\r\n\r\n",
    );
    body.extend_from_slice(TAP.as_bytes());
    body.extend_from_slice(
        b"\r\n--XyZ\r\nContent-Disposition: form-data; name=\"format\"\r\n\r\ntap\r\n--XyZ--\r\n",
    );
    let r = raw(
        &td,
        Method::POST,
        "/api/projects/proj/test-runs",
        Some("multipart/form-data; boundary=XyZ"),
        body,
        Some("proj/w1"),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.json());
    assert_eq!(r.json()["format"], "tap");
    assert_eq!(r.json()["name"], "node");
    assert_eq!(r.json()["counts"]["total"], 5);
    // base64 in the envelope.
    let b64 = base64::engine::general_purpose::STANDARD.encode(TAP);
    let v = push(
        &td,
        "proj/w1",
        json!({"data_base64": b64, "format": "auto"}),
    )
    .await;
    assert_eq!(v["format"], "tap");
    assert_eq!(v["file_name"], "results.tap");

    // Errors.
    let post = |b: Value| td.post_as("proj/w1", "/api/projects/proj/test-runs", b);
    post(json!({"content": "<html/>"}))
        .await
        .err(422, "invalid");
    post(json!({"content": CTRF, "format": "junit"}))
        .await
        .err(422, "invalid");
    post(json!({"content": "ok 1", "format": "xunit"}))
        .await
        .err(422, "invalid");
    post(json!({"content": "", "data_base64": ""}))
        .await
        .err(400, "bad_request");
    post(json!({"content": "ok 1", "colour": "red"}))
        .await
        .err(400, "bad_request");
    let xxe = "<?xml version=\"1.0\"?><!DOCTYPE t [<!ENTITY x SYSTEM \"file:///etc/passwd\">]><testsuites><testsuite name=\"&x;\"/></testsuites>";
    let r = post(json!({"content": xxe})).await;
    r.err(422, "invalid");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("DOCTYPE")
    );
    post(json!({"content": "ok 1", "name": "two\nlines"}))
        .await
        .ok();
    post(json!({"content": "ok 1", "name": "x".repeat(201)}))
        .await
        .err(422, "invalid");
    post(json!({"content": "ok 1", "cards": ["TER-99"]}))
        .await
        .err(404, "not_found");
    let big = vec![b' '; 20 * 1024 * 1024 + 1];
    let r = raw(
        &td,
        Method::POST,
        "/api/projects/proj/test-runs",
        Some("text/plain"),
        big,
        Some("proj/w1"),
    )
    .await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(r.json()["error"]["code"], "too_large");
    // Another project's agent may not upload or read.
    td.post_as(
        "other/x",
        "/api/projects/proj/test-runs",
        json!({"content": "ok 1"}),
    )
    .await
    .err(403, "forbidden");
    td.get_as("other/x", "/api/projects/proj/test-runs")
        .await
        .err(403, "forbidden");
    td.get_as("other/x", &format!("/api/test-runs/{}", id(&v)))
        .await
        .err(403, "forbidden");
    // A failed link stores nothing: 5 runs only.
    let all = td.get("/api/projects/proj/test-runs").await;
    assert_eq!(all.ok().as_array().unwrap().len(), 5);
}

#[tokio::test]
async fn compare_with_previous_and_with_a_run() {
    let td = setup().await;
    let first = push(
        &td,
        "proj/w1",
        json!({"name": "unit", "content": junit(&[("a", "passed"), ("b", "failed"), ("c", "failed"), ("d", "passed"), ("old", "passed")])}),
    )
    .await;
    // No previous run: no base.
    let c = td
        .get(&format!("/api/test-runs/{}/compare", id(&first)))
        .await;
    assert_eq!(c.ok()["base"], Value::Null);
    assert_eq!(c.ok()["counts"]["newly_failing"], 0);
    push(
        &td,
        "proj/w1",
        json!({"name": "other", "content": junit(&[("a", "failed")])}),
    )
    .await;
    let second = push(
        &td,
        "proj/w1",
        json!({"name": "unit", "content": junit(&[("a", "failed"), ("b", "passed"), ("c", "failed"), ("d", "skipped"), ("new", "passed")])}),
    )
    .await;
    let c = td
        .get(&format!(
            "/api/test-runs/{}/compare?with=previous",
            id(&second)
        ))
        .await;
    let c = c.ok();
    assert_eq!(c["base"]["id"], first["id"]);
    assert_eq!(
        c["counts"],
        json!({"newly_failing": 1, "fixed": 1, "still_failing": 1, "new_tests": 1, "removed_tests": 1})
    );
    assert_eq!(c["newly_failing"][0]["name"], "a");
    assert_eq!(c["newly_failing"][0]["status"], "failed");
    assert_eq!(c["newly_failing"][0]["base_status"], "passed");
    assert_eq!(c["newly_failing"][0]["message"], "a broke");
    assert_eq!(c["newly_failing"][0]["suite"], "s");
    assert_eq!(c["newly_failing"][0]["classname"], "c");
    assert_eq!(c["fixed"][0]["name"], "b");
    assert_eq!(c["still_failing"][0]["name"], "c");
    assert_eq!(c["new_tests"][0]["name"], "new");
    assert_eq!(c["new_tests"][0]["base_status"], Value::Null);
    assert_eq!(c["removed_tests"][0]["name"], "old");
    assert_eq!(c["removed_tests"][0]["status"], Value::Null);
    // With an explicit run, the other way round.
    let c = td
        .get(&format!(
            "/api/test-runs/{}/compare?with={}",
            id(&first),
            id(&second)
        ))
        .await;
    let c = c.ok();
    assert_eq!(c["counts"]["fixed"], 1);
    assert_eq!(c["fixed"][0]["name"], "a");
    assert_eq!(c["newly_failing"][0]["name"], "b");
    td.get(&format!(
        "/api/test-runs/{}/compare?with=01ARZ3NDEKTSV4RRFFQ69G5FAV",
        id(&first)
    ))
    .await
    .err(404, "not_found");
}

#[tokio::test]
async fn links_follow_card_rights_and_take_labels() {
    let td = setup().await;
    let mine = card(&td, "proj/w1", "Mine").await;
    let theirs = card(&td, "proj/w2", "Theirs").await;
    let run = push(&td, "proj/w1", json!({"content": "1..1\nok 1 - a\n"})).await;
    let url = format!("/api/test-runs/{}/links", id(&run));
    // A card of another agent: refused, and the upload stores nothing.
    td.post_as(
        "proj/w1",
        "/api/projects/proj/test-runs",
        json!({"content": "ok 1", "cards": [theirs]}),
    )
    .await
    .err(403, "forbidden");
    td.patch_as("proj/w1", &url, json!({"add_cards": [theirs]}))
        .await
        .err(403, "forbidden");
    let v = td
        .patch_as("proj/w1", &url, json!({"add_cards": [mine]}))
        .await;
    assert_eq!(v.ok()["cards"], json!([mine]));
    // An admin links any card.
    let v = td
        .patch_as("proj/lead", &url, json!({"add_cards": [theirs]}))
        .await;
    assert_eq!(v.ok()["cards"].as_array().unwrap().len(), 2);
    td.patch_as("proj/w1", &url, json!({}))
        .await
        .err(422, "invalid");
    td.patch_as("proj/w1", &url, json!({"add_cards": ["TER-77"]}))
        .await
        .err(404, "not_found");

    // Labels (TER-58): free text, any member of the project links them.
    let v = td
        .patch_as(
            "proj/w2",
            &url,
            json!({"add_labels": [" version:1.0.0 ", "version:vnext"]}),
        )
        .await;
    assert_eq!(v.ok()["labels"], json!(["version:1.0.0", "version:vnext"]));
    assert!(v.ok().get("versions").is_none(), "{}", v.ok());
    td.patch_as("proj/w1", &url, json!({"add_labels": [" "]}))
        .await
        .err(422, "invalid");
    let by_label = td
        .get("/api/projects/proj/test-runs?label=version:1.0.0")
        .await;
    assert_eq!(by_label.ok().as_array().unwrap().len(), 1);
    let by_label = td
        .get("/api/projects/proj/test-runs?label=version:9.9.9")
        .await;
    assert_eq!(by_label.ok().as_array().unwrap().len(), 0);
    let v = td
        .patch_as("proj/w1", &url, json!({"remove_labels": ["version:vnext"]}))
        .await;
    assert_eq!(v.ok()["labels"], json!(["version:1.0.0"]));
    // The version fields are gone: 400, never a silent ignore.
    td.patch_as("proj/w1", &url, json!({"add_versions": ["v1"]}))
        .await
        .err(400, "bad_request");
    td.get("/api/projects/proj/test-runs?version=v1")
        .await
        .err(400, "bad_request");
    // Unlink a card.
    let v = td
        .patch_as("proj/w1", &url, json!({"remove_cards": [mine]}))
        .await;
    assert_eq!(v.ok()["cards"], json!([theirs]));
    let ev = td.get("/api/events?kind=test_run").await;
    let changes: Vec<String> = ev
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["data"]["change"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        changes,
        [
            "created", "linked", "linked", "linked", "unlinked", "unlinked"
        ]
    );
    // A card of another project.
    let other_card = td
        .post_as(
            "other/x",
            "/api/projects/other/cards",
            json!({"title": "t"}),
        )
        .await
        .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    td.patch_as(
        "proj/lead",
        &url,
        json!({"add_cards": [other_card.clone()]}),
    )
    .await
    .err(403, "forbidden");
    td.patch(&url, json!({"add_cards": [other_card]}))
        .await
        .err(422, "invalid");
}

#[tokio::test]
async fn delete_rights_and_audit() {
    let td = setup().await;
    let run = push(&td, "proj/w1", json!({"content": "ok 1"})).await;
    let url = format!("/api/test-runs/{}", id(&run));
    td.req(Method::DELETE, &url, None, Some("proj/w2"))
        .await
        .err(403, "forbidden");
    let r = td.req(Method::DELETE, &url, None, Some("proj/w1")).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    td.get(&url).await.err(404, "not_found");
    // The creator's own delete is not audited; an admin's is.
    let run = push(&td, "proj/w1", json!({"content": "ok 1", "name": "keep"})).await;
    let url = format!("/api/test-runs/{}", id(&run));
    let r = td.req(Method::DELETE, &url, None, Some("proj/lead")).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let a = td.get("/api/audit?action=test_run.delete").await;
    let rows = a.ok().as_array().unwrap().clone();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["detail"]["name"], "keep");
    let ev = td.get("/api/events?kind=test_run").await;
    let deleted = ev
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["data"]["change"] == "deleted")
        .count();
    assert_eq!(deleted, 2);
    // The operator deletes too.
    let run = push(&td, "proj/w2", json!({"content": "ok 1"})).await;
    let r = td.delete(&format!("/api/test-runs/{}", id(&run))).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
}
