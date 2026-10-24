//! Artifacts: upload, dedupe, sniffing, signed serving, limits, and
//! retention (PLAN §15.2).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use base64::Engine;
use common::{TestDaemon, write};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::header::HeaderMap;
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::UnixStream;

struct Raw {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
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

async fn setup_with(f: impl FnOnce(&std::path::Path)) -> TestDaemon {
    let td = TestDaemon::with_config(f).await;
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

async fn setup() -> TestDaemon {
    setup_with(|_| {}).await
}

fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

async fn upload_json(td: &TestDaemon, agent: &str, body: Value) -> common::Resp {
    td.post_as(agent, "/api/projects/proj/artifacts", body)
        .await
}

async fn upload(td: &TestDaemon, agent: &str, name: &str, data: &[u8]) -> Value {
    upload_json(td, agent, json!({"name": name, "data_base64": b64(data)}))
        .await
        .ok()
        .clone()
}

fn tar_of(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for (path, data) in files {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, path, *data).unwrap();
    }
    b.into_inner().unwrap()
}

/// A tar with one entry whose name is written raw, to get past the
/// builder's own path checks.
fn evil_tar(name: &str) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_gnu();
    let gnu = h.as_gnu_mut().unwrap();
    gnu.name[..name.len()].copy_from_slice(name.as_bytes());
    h.set_size(3);
    h.set_mode(0o644);
    h.set_entry_type(tar::EntryType::Regular);
    h.set_cksum();
    b.append(&h, &b"bad"[..]).unwrap();
    b.into_inner().unwrap()
}

async fn mint(td: &TestDaemon, id: &str, body: Option<Value>) -> String {
    td.req(
        Method::POST,
        &format!("/api/artifacts/{id}/url"),
        body,
        Some("proj/w1"),
    )
    .await
    .ok()["url"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn fetch(td: &TestDaemon, url: &str) -> Raw {
    raw(td, Method::GET, url, None, Vec::new(), None).await
}

fn header<'a>(r: &'a Raw, name: &str) -> &'a str {
    r.headers
        .get(name)
        .map(|v| v.to_str().unwrap())
        .unwrap_or("")
}

#[tokio::test]
async fn upload_dedupe_sniff_and_serve_files() {
    let td = setup().await;
    let a = upload(&td, "proj/w1", "notes.txt", b"hello world").await;
    assert_eq!(a["id"], "ART-1");
    assert_eq!(a["key"], "proj/ART-1");
    assert_eq!(a["kind"], "file");
    assert_eq!(a["size"], 11);
    assert_eq!(a["content_type"], "text/plain; charset=utf-8");
    assert_eq!(a["renders"], true);
    assert_eq!(a["created_by_name"], "proj/w1");
    // The same bytes again: a new record, one blob.
    let b = upload(&td, "proj/w2", "copy.txt", b"hello world").await;
    assert_eq!(b["id"], "ART-2");
    assert_eq!(b["sha256"], a["sha256"]);
    let blobs: Vec<_> = std::fs::read_dir(td.state.join("artifacts"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| !n.ends_with(".key"))
        .collect();
    assert_eq!(blobs, [a["sha256"].as_str().unwrap()]);

    // Multipart, with a claimed type that the bytes do not back.
    let png = b"\x89PNG\r\n\x1a\nnot really a png";
    let mut body = Vec::new();
    body.extend_from_slice(
        b"--XyZ\r\nContent-Disposition: form-data; name=\"kind\"\r\n\r\nfile\r\n\
          --XyZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"shot.html\"\r\n\
          Content-Type: text/html\r\n\r\n",
    );
    body.extend_from_slice(png);
    body.extend_from_slice(b"\r\n--XyZ--\r\n");
    let r = raw(
        &td,
        Method::POST,
        "/api/projects/proj/artifacts",
        Some("multipart/form-data; boundary=XyZ"),
        body,
        Some("proj/w1"),
    )
    .await;
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    let m: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(m["name"], "shot.html");
    assert_eq!(m["content_type"], "image/png");

    // An executable never renders, whatever its name.
    let exe = upload(&td, "proj/w1", "report.html", b"\x7fELF\x02\x01\x01rest").await;
    assert_eq!(exe["content_type"], "application/octet-stream");
    assert_eq!(exe["renders"], false);
    let url = mint(&td, "ART-4", None).await;
    let r = fetch(&td, &url).await;
    assert_eq!(r.status, 200);
    assert_eq!(header(&r, "content-type"), "application/octet-stream");
    assert!(header(&r, "content-disposition").starts_with("attachment"));

    // A text file serves inline through a signed URL.
    let url = mint(&td, "ART-1", Some(json!({"ttl": 60}))).await;
    assert!(url.starts_with(&format!("/a/{}/notes.txt?exp=", a["uid"].as_str().unwrap())));
    let r = fetch(&td, &url).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body, b"hello world");
    assert!(header(&r, "content-security-policy").starts_with("sandbox"));
    assert_eq!(header(&r, "x-content-type-options"), "nosniff");
    assert!(r.headers.get("set-cookie").is_none());
    assert!(r.headers.get("content-disposition").is_none());

    // Bad input.
    upload_json(&td, "proj/w1", json!({"name": "a", "data_base64": "!!"}))
        .await
        .err(400, "bad_request");
    upload_json(
        &td,
        "proj/w1",
        json!({"name": "a/b", "data_base64": b64(b"x")}),
    )
    .await
    .err(422, "invalid");
    upload_json(
        &td,
        "other/x",
        json!({"name": "a", "data_base64": b64(b"x")}),
    )
    .await
    .err(403, "forbidden");
    let r = td.get("/api/projects/proj/artifacts").await;
    assert_eq!(r.ok().as_array().unwrap().len(), 4);
    let u = td.get("/api/projects/proj/artifacts/usage").await;
    assert_eq!(u.ok()["count"], 4);

    // Only the author or an admin deletes.
    td.req(
        Method::DELETE,
        "/api/artifacts/ART-1",
        None,
        Some("proj/w2"),
    )
    .await
    .err(403, "forbidden");
    let r = td
        .req(
            Method::DELETE,
            "/api/artifacts/ART-1",
            None,
            Some("proj/w1"),
        )
        .await;
    assert_eq!(r.status.as_u16(), 204);
    // ART-2 still uses the blob.
    assert!(
        td.state
            .join("artifacts")
            .join(a["sha256"].as_str().unwrap())
            .is_file()
    );
    let r = td
        .req(
            Method::DELETE,
            "/api/artifacts/ART-2",
            None,
            Some("proj/lead"),
        )
        .await;
    assert_eq!(r.status.as_u16(), 204);
    assert!(
        !td.state
            .join("artifacts")
            .join(a["sha256"].as_str().unwrap())
            .exists()
    );
    let audit = td.get("/api/audit?action=artifact.delete").await;
    assert_eq!(audit.ok().as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn bundle_serves_index_through_a_signed_url_only() {
    let td = setup().await;
    let tar = tar_of(&[
        ("index.html", b"<html><link href=\"style.css\"></html>"),
        ("style.css", b"body{}"),
        ("sub/index.html", b"<p>sub</p>"),
    ]);
    let r = upload_json(
        &td,
        "proj/w1",
        json!({"name": "site", "kind": "bundle", "data_base64": b64(&tar)}),
    )
    .await;
    let a = r.ok().clone();
    assert_eq!(a["kind"], "bundle");
    // A bundle renders: the viewer shows it in a sandboxed frame.
    assert_eq!(a["renders"], true);
    let uid = a["uid"].as_str().unwrap().to_owned();

    let url = mint(&td, "ART-1", None).await;
    assert!(url.starts_with(&format!("/a/{uid}/_s/")), "{url}");
    assert!(url.ends_with('/'));
    let r = fetch(&td, &url).await;
    assert_eq!(r.status, 200);
    assert!(String::from_utf8_lossy(&r.body).contains("style.css"));
    assert!(header(&r, "content-type").starts_with("text/html"));
    assert!(header(&r, "content-security-policy").starts_with("sandbox"));
    assert!(r.headers.get("set-cookie").is_none());
    // Relative links work, because the signature is in the path.
    let r = fetch(&td, &format!("{url}style.css")).await;
    assert_eq!(r.status, 200);
    assert!(header(&r, "content-type").starts_with("text/css"));
    let r = fetch(&td, &format!("{url}sub/")).await;
    assert_eq!(r.body, b"<p>sub</p>");
    let r = fetch(&td, &format!("{url}missing.js")).await;
    assert_eq!(r.status, 404);
    let r = fetch(&td, &format!("{url}../../etc/passwd")).await;
    assert_ne!(r.status, 200);

    // No signature, a bad one, or another artifact's: refused.
    let r = fetch(&td, &format!("/a/{uid}/")).await;
    assert_eq!(r.status, 403);
    let parts: Vec<&str> = url.split('/').collect();
    let (exp, sig) = (parts[4], parts[5]);
    let mut bad = sig.to_owned();
    bad.replace_range(0..1, if sig.starts_with('0') { "1" } else { "0" });
    let r = fetch(&td, &format!("/a/{uid}/_s/{exp}/{bad}/")).await;
    assert_eq!(r.status, 403);
    let later: i64 = exp.parse::<i64>().unwrap() + 1000;
    let r = fetch(&td, &format!("/a/{uid}/_s/{later}/{sig}/")).await;
    assert_eq!(r.status, 403);
    let r = fetch(&td, &format!("/a/{uid}/index.html?exp={exp}&sig={sig}")).await;
    assert_eq!(r.status, 200, "the query form works too");
    // Expired.
    let short = mint(&td, "ART-1", Some(json!({"ttl": 1}))).await;
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
    let r = fetch(&td, &short).await;
    assert_eq!(r.status, 403);

    // The serving router alone works the same way, for its own listener.
    let state = td.daemon.as_ref().unwrap().state.clone();
    let app = terrariumd::api::artifact_router(state);
    let fresh = mint(&td, "ART-1", None).await;
    let resp = tower::ServiceExt::oneshot(
        app,
        Request::builder()
            .uri(&fresh)
            .body(axum::body::Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);

    // Bad bundles.
    upload_json(
        &td,
        "proj/w1",
        json!({"name": "s", "kind": "bundle", "data_base64": b64(&tar_of(&[("a.html", b"x")]))}),
    )
    .await
    .err(422, "invalid");
    upload_json(
        &td,
        "proj/w1",
        json!({"name": "s", "kind": "bundle", "data_base64": b64(&evil_tar("../evil.html"))}),
    )
    .await
    .err(422, "invalid");
    upload_json(
        &td,
        "proj/w1",
        json!({"name": "s", "kind": "bundle", "data_base64": b64(b"not a tar at all")}),
    )
    .await
    .err(422, "invalid");
}

#[tokio::test]
async fn per_file_limit_and_project_quota() {
    let td = setup_with(|c| {
        write(
            c,
            "daemon.toml",
            "[artifacts]\nmax_file_bytes = 1000\nproject_quota_bytes = 2500\n",
        )
    })
    .await;
    let r = upload_json(
        &td,
        "proj/w1",
        json!({"name": "big", "data_base64": b64(&[b'a'; 1001])}),
    )
    .await;
    r.err(413, "too_large");
    upload(&td, "proj/w1", "a", &[b'a'; 1000]).await;
    upload(&td, "proj/w1", "b", &[b'b'; 1000]).await;
    let r = upload_json(
        &td,
        "proj/w1",
        json!({"name": "c", "data_base64": b64(&[b'c'; 600])}),
    )
    .await;
    r.err(413, "too_large");
    assert_eq!(r.body["error"]["details"]["used_bytes"], 2000);
    let u = td.get("/api/projects/proj/artifacts/usage").await;
    let u = u.ok();
    assert_eq!(u["used_bytes"], 2000);
    assert_eq!(u["quota_bytes"], 2500);
    assert_eq!(u["max_file_bytes"], 1000);
}

#[tokio::test]
async fn attach_and_prune_keep_the_right_things() {
    let td = setup().await;
    // ART-1 unattached; ART-2 on a released version; ART-3 embedded in a
    // live page; ART-4 embedded in a page that is deleted; ART-5 on a card.
    for (i, n) in ["loose", "release", "embedded", "dead-page", "card"]
        .iter()
        .enumerate()
    {
        upload(&td, "proj/lead", n, format!("data {i}").as_bytes()).await;
    }
    td.post_as(
        "proj/lead",
        "/api/projects/proj/versions",
        json!({"name": "v1.0.0", "artifacts": ["ART-2"]}),
    )
    .await
    .ok();
    td.post_empty_as("proj/lead", "/api/versions/v1.0.0/submit")
        .await
        .ok();
    td.post_empty("/api/versions/proj%2Fv1.0.0/approve")
        .await
        .ok();
    td.post_empty_as("proj/lead", "/api/versions/v1.0.0/release")
        .await
        .ok();
    td.post_as(
        "proj/w1",
        "/api/projects/proj/wiki",
        json!({"path": "live", "body": "![x](artifact:ART-3)", "summary": "s"}),
    )
    .await
    .ok();
    let dead = td
        .post_as(
            "proj/w1",
            "/api/projects/proj/wiki",
            json!({"path": "dead", "body": "```artifact ART-4```", "summary": "s"}),
        )
        .await;
    let dead_id = dead.ok()["id"].as_str().unwrap().to_owned();
    td.post_as("proj/w1", "/api/projects/proj/cards", json!({"title": "c"}))
        .await
        .ok();
    let r = td
        .post_as(
            "proj/w1",
            "/api/artifacts/ART-5/links",
            json!({"kind": "card", "ref": "TER-1"}),
        )
        .await;
    assert_eq!(r.ok()["links"][0]["ref"], "TER-1");
    td.post_as(
        "proj/w1",
        "/api/artifacts/ART-5/links",
        json!({"kind": "shelf", "ref": "x"}),
    )
    .await
    .err(422, "invalid");
    let a3 = td.get("/api/artifacts/proj%2FART-3").await;
    assert_eq!(a3.ok()["embedded_in"], json!(["proj/live"]));
    let r = td
        .req(
            Method::DELETE,
            &format!("/api/wiki/{dead_id}"),
            None,
            Some("proj/lead"),
        )
        .await;
    assert_eq!(r.status.as_u16(), 204);
    // An artifact on a released version cannot be deleted or re-attached.
    td.req(
        Method::DELETE,
        "/api/artifacts/ART-2",
        None,
        Some("proj/lead"),
    )
    .await
    .err(409, "conflict");
    td.post_as(
        "proj/lead",
        "/api/artifacts/ART-1/links",
        json!({"kind": "version", "ref": "v1.0.0"}),
    )
    .await
    .err(409, "conflict");

    // Only a sysadmin or the operator prunes.
    td.post_as("proj/lead", "/api/artifacts/prune", json!({}))
        .await
        .err(403, "forbidden");
    let day = chrono::Duration::days(1);
    let at = |d: chrono::Duration| {
        (chrono::Utc::now() + d).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    };
    let keys = |v: &Value, field: &str| -> Vec<String> {
        v[field]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["key"].as_str().unwrap().to_owned())
            .collect()
    };
    // Young: nothing happens.
    let r = td
        .post(
            "/api/artifacts/prune",
            json!({"dry_run": false, "as_of": at(day * 10)}),
        )
        .await;
    assert_eq!(keys(r.ok(), "warned"), Vec::<String>::new());
    assert_eq!(r.ok()["kept"], 5);
    // Dry run past the age: it says what it would warn, and changes nothing.
    let r = td
        .post("/api/artifacts/prune", json!({"as_of": at(day * 40)}))
        .await;
    assert_eq!(r.ok()["dry_run"], true);
    assert_eq!(keys(r.ok(), "warned"), ["proj/ART-1", "proj/ART-4"]);
    let audit = td.get("/api/audit?action=artifact.prune_warning").await;
    assert_eq!(audit.ok(), &json!([]));
    // First real run: the warning, a day ahead.
    let r = td
        .post(
            "/api/artifacts/prune",
            json!({"dry_run": false, "as_of": at(day * 40)}),
        )
        .await;
    assert_eq!(keys(r.ok(), "warned"), ["proj/ART-1", "proj/ART-4"]);
    assert_eq!(keys(r.ok(), "pruned"), Vec::<String>::new());
    let audit = td.get("/api/audit?action=artifact.prune_warning").await;
    assert_eq!(audit.ok().as_array().unwrap().len(), 2);
    // Less than a day after the warning: still kept.
    let r = td
        .post(
            "/api/artifacts/prune",
            json!({"dry_run": false, "as_of": at(day * 40 + chrono::Duration::hours(12))}),
        )
        .await;
    assert_eq!(keys(r.ok(), "pruned"), Vec::<String>::new());
    // A day later: pruned, with the blobs.
    let sha1 = td.get("/api/artifacts/proj%2FART-1").await.ok()["sha256"]
        .as_str()
        .unwrap()
        .to_owned();
    let r = td
        .post(
            "/api/artifacts/prune",
            json!({"dry_run": false, "as_of": at(day * 41 + chrono::Duration::hours(1))}),
        )
        .await;
    assert_eq!(keys(r.ok(), "pruned"), ["proj/ART-1", "proj/ART-4"]);
    assert_eq!(r.ok()["kept"], 3);
    td.get("/api/artifacts/proj%2FART-1")
        .await
        .err(404, "not_found");
    for k in ["ART-2", "ART-3", "ART-5"] {
        td.get(&format!("/api/artifacts/proj%2F{k}")).await.ok();
    }
    assert!(!td.state.join("artifacts").join(sha1).exists());
    let audit = td.get("/api/audit?action=artifact.prune").await;
    assert_eq!(audit.ok().as_array().unwrap().len(), 2);
    let ev = td.get("/api/events?kind=artifact.pruned").await;
    assert_eq!(ev.ok().as_array().unwrap().len(), 2);
}
