//! The files of a card (docs/api.md §8.4): the upload forms, the rights,
//! the names, the types and the content check, the download headers (the
//! SVG rule), the versions and the soft delete, the events and the audit
//! rows, the prune, and the delivery into the workspace of the assignee at
//! the start and while it runs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::TestDaemon;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::header::HeaderMap;
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::UnixStream;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDRfake-png-body";
const SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg"><script>alert(1)</script></svg>"#;

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
    agent: Option<&str>,
    ct: Option<&str>,
    body: Vec<u8>,
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
    if let Some(a) = agent {
        req = req.header("x-terrarium-agent", a);
    }
    if let Some(ct) = ct {
        req = req.header("content-type", ct);
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

/// `POST /api/cards/{card}/files` in the JSON form.
async fn attach(
    td: &TestDaemon,
    card: &str,
    agent: Option<&str>,
    name: &str,
    data: &[u8],
    replace: bool,
) -> Raw {
    use base64::Engine;
    let body = json!({
        "name": name,
        "data_base64": base64::engine::general_purpose::STANDARD.encode(data),
        "replace": replace,
    });
    raw(
        td,
        Method::POST,
        &format!("/api/cards/{card}/files"),
        agent,
        Some("application/json"),
        body.to_string().into_bytes(),
    )
    .await
}

async fn card(td: &TestDaemon, project: &str, agent: Option<&str>, title: &str) -> String {
    let r = td
        .req(
            Method::POST,
            &format!("/api/projects/{project}/cards"),
            Some(json!({"title": title})),
            agent,
        )
        .await;
    r.ok()["id"].as_str().unwrap().to_owned()
}

/// Poll until `f` holds. A deadline, not a count of tries: on a loaded
/// machine a delivery at an agent start can take seconds.
async fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(60) {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out: {what}");
}

#[tokio::test]
async fn upload_forms_list_download_and_headers() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let c = card(&td, "p", None, "Spec").await;

    // JSON.
    let r = attach(&td, &c, None, "spec.md", b"# Spec\n", false).await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.json());
    let f = r.json();
    assert_eq!(f["name"], "spec.md");
    assert_eq!(f["version"], 1);
    assert_eq!(f["size"], 7);
    assert_eq!(f["outcome"], "added");
    assert_eq!(f["media_type"], "text/markdown; charset=utf-8");
    assert_eq!(f["created_by"], "operator");

    // Raw with `?name=`.
    let r = raw(
        &td,
        Method::POST,
        &format!("/api/cards/{c}/files?name=shot.png"),
        None,
        Some("image/png"),
        PNG.to_vec(),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.json());
    assert_eq!(r.json()["media_type"], "image/png");

    // Multipart.
    let boundary = "XyZ";
    let mut mp = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"data.json\"\r\n\
         Content-Type: application/json\r\n\r\n"
    )
    .into_bytes();
    mp.extend_from_slice(br#"{"a": 1}"#);
    mp.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let r = raw(
        &td,
        Method::POST,
        &format!("/api/cards/{c}/files"),
        None,
        Some(&format!("multipart/form-data; boundary={boundary}")),
        mp,
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.json());
    assert_eq!(r.json()["name"], "data.json");

    // The list: by name.
    let list = td.get(&format!("/api/cards/{c}/files")).await.ok().clone();
    let names: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["data.json", "shot.png", "spec.md"]);
    // The card ID in any case.
    let lower = c.to_ascii_lowercase();
    assert_eq!(
        td.get(&format!("/api/cards/{lower}/files")).await.ok(),
        &list
    );

    // The download: an attachment, nosniff, the sandbox CSP.
    let r = raw(
        &td,
        Method::GET,
        &format!("/api/cards/{c}/files/spec.md"),
        None,
        None,
        vec![],
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body, b"# Spec\n");
    let h = |k: &str| r.headers.get(k).unwrap().to_str().unwrap().to_owned();
    assert_eq!(h("content-type"), "text/markdown; charset=utf-8");
    assert_eq!(h("content-disposition"), "attachment; filename=\"spec.md\"");
    assert_eq!(h("x-content-type-options"), "nosniff");
    assert!(
        h("content-security-policy").starts_with("sandbox"),
        "{}",
        h("content-security-policy")
    );
    assert_eq!(h("x-terrarium-file-version"), "1");
    // A name in another case finds the file.
    let r = raw(
        &td,
        Method::GET,
        &format!("/api/cards/{c}/files/SPEC.MD"),
        None,
        None,
        vec![],
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    // `format=json`.
    let j = td
        .get(&format!("/api/cards/{c}/files/shot.png?format=json"))
        .await
        .ok()
        .clone();
    assert_eq!(j["card"], c.as_str());
    assert_eq!(j["version"], 1);
    use base64::Engine;
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(j["data_base64"].as_str().unwrap())
            .unwrap(),
        PNG
    );
    td.get(&format!("/api/cards/{c}/files/nope.md"))
        .await
        .err(404, "not_found");
    td.get(&format!("/api/cards/{c}/files/spec.md?format=xml"))
        .await
        .err(422, "invalid");

    // The SVG rule: it is stored, and it downloads as an attachment in a
    // sandbox, never as a page of the daemon origin.
    let r = attach(&td, &c, None, "logo.svg", SVG, false).await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.json());
    let r = raw(
        &td,
        Method::GET,
        &format!("/api/cards/{c}/files/logo.svg"),
        None,
        None,
        vec![],
    )
    .await;
    let h = |k: &str| r.headers.get(k).unwrap().to_str().unwrap().to_owned();
    assert_eq!(h("content-type"), "image/svg+xml");
    assert!(h("content-disposition").starts_with("attachment"));
    let csp = h("content-security-policy");
    assert!(
        csp.contains("sandbox") && csp.contains("default-src 'none'"),
        "{csp}"
    );
    assert!(!csp.contains("script-src"), "{csp}");
    assert_eq!(h("x-content-type-options"), "nosniff");
}

#[tokio::test]
async fn names_types_and_caps() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let c = card(&td, "p", None, "Caps").await;
    let code = |r: &Raw| r.json()["error"]["code"].as_str().unwrap_or("").to_owned();

    for (name, data, status, want) in [
        ("../x.md", &b"x"[..], 422, "invalid"),
        (".hidden.md", b"x", 422, "invalid"),
        ("index.md", b"x", 422, "invalid"),
        ("a b?.md", b"x", 422, "invalid"),
        ("run.sh", b"#!/bin/sh", 415, "unsupported_media_type"),
        ("page.html", b"<html>", 415, "unsupported_media_type"),
        ("fake.png", b"GIF89a.....", 415, "unsupported_media_type"),
        ("bin.txt", b"a\0b", 415, "unsupported_media_type"),
        ("latin1.md", b"caf\xe9", 415, "unsupported_media_type"),
        ("bad.json", b"{nope", 422, "invalid"),
        (
            "not.svg",
            b"<html><script></script></html>",
            415,
            "unsupported_media_type",
        ),
        ("doc.pdf", b"PK\x03\x04", 415, "unsupported_media_type"),
        ("empty.md", b"", 422, "invalid"),
    ] {
        let r = attach(&td, &c, None, name, data, false).await;
        assert_eq!(r.status.as_u16(), status, "{name}: {:?}", r.json());
        assert_eq!(code(&r), want, "{name}");
    }
    for (name, data) in [
        ("a.txt", &b"text"[..]),
        ("b.yaml", b"a: 1\n"),
        ("c.csv", b"a,b\n"),
        ("d.jpg", &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]),
        ("e.gif", b"GIF89a.."),
        ("f.webp", b"RIFF\0\0\0\0WEBPVP8 "),
        ("g.pdf", b"%PDF-1.7\n"),
        ("Notes v2+final.MD", b"# n"),
    ] {
        let r = attach(&td, &c, None, name, data, false).await;
        assert_eq!(r.status, StatusCode::CREATED, "{name}: {:?}", r.json());
    }
    // A name is unique with no regard to case.
    let r = attach(&td, &c, None, "A.TXT", b"other", false).await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.json());
    assert_eq!(r.json()["error"]["details"]["name"], "a.txt");
}

#[tokio::test]
async fn versions_soft_delete_events_audit_and_prune() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let c = card(&td, "p", None, "Versions").await;
    assert_eq!(
        attach(&td, &c, None, "spec.md", b"v1", false).await.status,
        StatusCode::CREATED
    );
    let r = attach(&td, &c, None, "spec.md", b"v2", true).await;
    assert_eq!(r.status, StatusCode::CREATED);
    assert_eq!(r.json()["outcome"], "replaced");
    assert_eq!(r.json()["version"], 2);
    assert_eq!(r.json()["versions"], 2);
    // The same bytes again: no new version.
    let r = attach(&td, &c, None, "spec.md", b"v2", true).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["outcome"], "unchanged");
    let v = td
        .get(&format!("/api/cards/{c}/files/spec.md/versions"))
        .await
        .ok()
        .clone();
    let vs: Vec<i64> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["version"].as_i64().unwrap())
        .collect();
    assert_eq!(vs, [2, 1]);
    let r = raw(
        &td,
        Method::GET,
        &format!("/api/cards/{c}/files/spec.md?version=1"),
        None,
        None,
        vec![],
    )
    .await;
    assert_eq!(r.body, b"v1");
    let r = raw(
        &td,
        Method::GET,
        &format!("/api/cards/{c}/files/spec.md"),
        None,
        None,
        vec![],
    )
    .await;
    assert_eq!(r.body, b"v2");
    td.get(&format!("/api/cards/{c}/files/spec.md?version=9"))
        .await
        .err(404, "not_found");

    // The remove: a soft delete of every version.
    let r = td.delete(&format!("/api/cards/{c}/files/spec.md")).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert!(
        td.get(&format!("/api/cards/{c}/files"))
            .await
            .ok()
            .as_array()
            .unwrap()
            .is_empty()
    );
    td.get(&format!("/api/cards/{c}/files/spec.md/versions"))
        .await
        .err(404, "not_found");
    td.delete(&format!("/api/cards/{c}/files/spec.md"))
        .await
        .err(404, "not_found");
    // The name again: a new version number, with no `replace`.
    let r = attach(&td, &c, None, "spec.md", b"v3", false).await;
    assert_eq!(r.status, StatusCode::CREATED);
    assert_eq!(r.json()["version"], 3);
    assert_eq!(r.json()["versions"], 1);

    // The events, in the card history.
    let ev = td.get(&format!("/api/cards/{c}/events")).await.ok().clone();
    let kinds: Vec<&str> = ev
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .filter(|k| k.starts_with("card.file_"))
        .collect();
    assert_eq!(
        kinds,
        [
            "card.file_added",
            "card.file_replaced",
            "card.file_removed",
            "card.file_added"
        ]
    );
    let removed = ev
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "card.file_removed")
        .unwrap();
    assert_eq!(removed["data"]["name"], "spec.md");
    assert_eq!(removed["data"]["versions"], 2);
    assert_eq!(removed["card"], c.as_str());
    // The audit rows.
    let audit = td.get("/api/audit?limit=100").await.ok().clone();
    let actions: Vec<&str> = audit
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["action"].as_str())
        .filter(|a| a.starts_with("card.file"))
        .collect();
    for want in ["card.file_add", "card.file_replace", "card.file_remove"] {
        assert!(actions.contains(&want), "{want}: {actions:?}");
    }

    // The prune: the removed rows stay for the trash days; after them the
    // rows and the blobs that no row names go.
    let state = td.daemon.as_ref().unwrap().state.clone();
    let blobs = |d: &Path| std::fs::read_dir(d.join("card-files")).unwrap().count();
    assert_eq!(blobs(&td.state), 3);
    let now = std::time::SystemTime::now();
    assert_eq!(
        terrariumd::api::card_files::prune(&state, now)
            .await
            .unwrap(),
        (0, 0)
    );
    let later = now + Duration::from_secs(8 * 86_400);
    let (rows, gone) = terrariumd::api::card_files::prune(&state, later)
        .await
        .unwrap();
    assert_eq!((rows, gone), (2, 2), "v1 and v2 of the removed file");
    assert_eq!(blobs(&td.state), 1);
    let r = raw(
        &td,
        Method::GET,
        &format!("/api/cards/{c}/files/spec.md"),
        None,
        None,
        vec![],
    )
    .await;
    assert_eq!(r.body, b"v3");
}

#[tokio::test]
async fn the_rights_are_those_of_a_card_edit() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.project("q").await;
    td.agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await;
    td.agent("p", "w1", json!({})).await;
    td.agent("p", "w2", json!({})).await;
    td.agent("q", "other", json!({})).await;
    // An unassigned card that w1 made: its own.
    let mine = card(&td, "p", Some("p/w1"), "Mine").await;
    // A card assigned to w2.
    let theirs = card(&td, "p", None, "Theirs").await;
    td.post(
        &format!("/api/cards/{theirs}/assign"),
        json!({"assignee": "w2"}),
    )
    .await
    .ok();
    let status = |r: Raw| r.status.as_u16();

    // A change: the creator of an unassigned card, the assignee, an admin,
    // the operator. Not another member, not another project.
    assert_eq!(
        status(attach(&td, &mine, Some("p/w1"), "a.md", b"a", false).await),
        201
    );
    assert_eq!(
        status(attach(&td, &mine, Some("p/w2"), "b.md", b"b", false).await),
        403
    );
    assert_eq!(
        status(attach(&td, &mine, Some("q/other"), "c.md", b"c", false).await),
        403
    );
    assert_eq!(
        status(attach(&td, &theirs, Some("p/w2"), "d.md", b"d", false).await),
        201
    );
    assert_eq!(
        status(attach(&td, &theirs, Some("p/w1"), "e.md", b"e", false).await),
        403
    );
    assert_eq!(
        status(attach(&td, &theirs, Some("p/lead"), "f.md", b"f", false).await),
        201
    );
    assert_eq!(
        status(attach(&td, &theirs, None, "g.md", b"g", false).await),
        201
    );

    // A read: every member of the project, the operator; not another project.
    for who in [Some("p/w1"), Some("p/w2"), Some("p/lead"), None] {
        let r = raw(
            &td,
            Method::GET,
            &format!("/api/cards/{theirs}/files"),
            who,
            None,
            vec![],
        )
        .await;
        assert_eq!(r.status, StatusCode::OK, "{who:?}");
        let r = raw(
            &td,
            Method::GET,
            &format!("/api/cards/{theirs}/files/d.md"),
            who,
            None,
            vec![],
        )
        .await;
        assert_eq!(r.body, b"d", "{who:?}");
    }
    let r = raw(
        &td,
        Method::GET,
        &format!("/api/cards/{theirs}/files"),
        Some("q/other"),
        None,
        vec![],
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = raw(
        &td,
        Method::GET,
        &format!("/api/cards/{theirs}/files/d.md"),
        Some("q/other"),
        None,
        vec![],
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    // A remove: the edit rights too.
    let r = raw(
        &td,
        Method::DELETE,
        &format!("/api/cards/{theirs}/files/d.md"),
        Some("p/w1"),
        None,
        vec![],
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = raw(
        &td,
        Method::DELETE,
        &format!("/api/cards/{theirs}/files/d.md"),
        Some("p/w2"),
        None,
        vec![],
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);

    // A done card: only an admin or the operator.
    td.post(
        &format!("/api/cards/{theirs}/move"),
        json!({"column": "done"}),
    )
    .await
    .ok();
    assert_eq!(
        status(attach(&td, &theirs, Some("p/w2"), "h.md", b"h", false).await),
        403
    );
    assert_eq!(
        status(attach(&td, &theirs, Some("p/lead"), "h.md", b"h", false).await),
        201
    );
    // An archived card: no change at all.
    td.delete(&format!("/api/cards/{mine}")).await;
    assert_eq!(
        status(attach(&td, &mine, None, "z.md", b"z", false).await),
        409
    );
    // A refused caller gets no blob into the store.
    let blobs = std::fs::read_dir(td.state.join("card-files"))
        .unwrap()
        .count();
    assert_eq!(
        status(attach(&td, &mine, Some("q/other"), "y.md", b"never stored", false).await),
        403
    );
    assert_eq!(
        std::fs::read_dir(td.state.join("card-files"))
            .unwrap()
            .count(),
        blobs
    );
}

fn cards_dir(workspace: &Path, card: &str) -> PathBuf {
    workspace.join(".terrarium/cards").join(card)
}

#[tokio::test]
async fn delivery_at_start_and_while_running() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let a = td.agent("p", "w1", json!({})).await;
    let id = a["id"].as_str().unwrap().to_owned();
    let ws = PathBuf::from(a["workspace"].as_str().unwrap());
    std::fs::create_dir_all(ws.join(".git/info")).unwrap();
    let c1 = card(&td, "p", None, "First").await;
    let c2 = card(&td, "p", None, "Second").await;
    assert_eq!(
        attach(&td, &c1, None, "spec.md", b"# one", false)
            .await
            .status,
        StatusCode::CREATED
    );
    // A stale dir of an old card.
    std::fs::create_dir_all(cards_dir(&ws, "P-99")).unwrap();
    std::fs::write(cards_dir(&ws, "P-99").join("old.md"), b"old").unwrap();
    // The assignment wakes the agent (PLAN §18); its start delivers.
    td.post(
        &format!("/api/cards/{c1}/assign"),
        json!({"assignee": "w1"}),
    )
    .await
    .ok();
    let f = cards_dir(&ws, &c1).join("spec.md");
    // One sync writes the files, then `index.md`: wait for the index too.
    let index_path = cards_dir(&ws, &c1).join("index.md");
    wait_for("the delivery at start", || {
        f.exists() && index_path.exists()
    })
    .await;
    assert_eq!(std::fs::read(&f).unwrap(), b"# one");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
        0o444
    );
    let index = std::fs::read_to_string(&index_path).unwrap();
    assert!(index.contains(&format!("# {c1}: First")), "{index}");
    assert!(index.contains("| spec.md | 1 |"), "{index}");
    wait_for("the stale card dir goes", || {
        !cards_dir(&ws, "P-99").exists()
    })
    .await;
    // The sync adds the git exclude after it writes the index: wait for it
    // too (TER-188: a slow runner read it before the sync was done).
    let exclude_path = ws.join(".git/info/exclude");
    wait_for("the git exclude of the card files", || {
        std::fs::read_to_string(&exclude_path).is_ok_and(|t| t.contains("/.terrarium/"))
    })
    .await;
    td.wait_status(&id, "idle").await;

    // A file of a stopped agent waits for its next start: no delivery, no
    // mail (a mail would wake it).
    td.post_empty(&format!("/api/agents/{id}/stop")).await.ok();
    td.wait_status(&id, "stopped").await;
    attach(&td, &c1, None, "later.md", b"later", false).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let later = cards_dir(&ws, &c1).join("later.md");
    assert!(!later.exists());
    assert!(
        !mail_subjects(&td)
            .await
            .iter()
            .any(|s| s.contains("later.md"))
    );
    assert_eq!(
        td.get(&format!("/api/agents/{id}")).await.ok()["status"],
        "stopped"
    );
    td.post_empty(&format!("/api/agents/{id}/start")).await.ok();
    wait_for("the delivery at the next start", || later.exists()).await;
    td.wait_status(&id, "idle").await;

    // A new file while it runs: the folder and a notice.
    assert_eq!(
        attach(&td, &c1, None, "plan.txt", b"plan", false)
            .await
            .status,
        StatusCode::CREATED
    );
    let plan = cards_dir(&ws, &c1).join("plan.txt");
    wait_for("the new file", || plan.exists()).await;
    wait_for_mail(&td, &format!("{c1}: file plan.txt added")).await;
    // A replace.
    attach(&td, &c1, None, "plan.txt", b"plan v2", true).await;
    wait_for("the new version", || {
        std::fs::read(&plan).is_ok_and(|b| b == b"plan v2")
    })
    .await;
    // A remove.
    td.delete(&format!("/api/cards/{c1}/files/plan.txt")).await;
    wait_for("the removed file", || !plan.exists()).await;
    wait_for_mail(&td, &format!("{c1}: file plan.txt replaced")).await;
    wait_for_mail(&td, &format!("{c1}: file plan.txt removed")).await;

    // An assignment of a card with files: delivered, with a notice.
    attach(&td, &c2, None, "brief.md", b"brief", false).await;
    td.post(
        &format!("/api/cards/{c2}/assign"),
        json!({"assignee": "w1"}),
    )
    .await
    .ok();
    let brief = cards_dir(&ws, &c2).join("brief.md");
    wait_for("the assigned card", || brief.exists()).await;
    wait_for_mail(&td, &format!("{c2}: card files")).await;
    // The agent's own file: delivered, no notice.
    let before = mail_subjects(&td).await.len();
    let r = attach(&td, &c2, Some("p/w1"), "mine.md", b"mine", false).await;
    assert_eq!(r.status, StatusCode::CREATED);
    let mine = cards_dir(&ws, &c2).join("mine.md");
    wait_for("the agent's own file", || mine.exists()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(mail_subjects(&td).await.len(), before);

    // A symlink planted in the place of a delivered file is replaced, and
    // its target is not touched.
    let outside = td.dir.path().join("outside.txt");
    std::fs::write(&outside, b"keep").unwrap();
    std::fs::remove_file(&brief).unwrap();
    std::os::unix::fs::symlink(&outside, &brief).unwrap();
    attach(&td, &c2, None, "brief.md", b"brief v2", true).await;
    wait_for("the replaced symlink", || {
        brief.symlink_metadata().is_ok_and(|m| m.is_file())
    })
    .await;
    assert_eq!(std::fs::read(&brief).unwrap(), b"brief v2");
    assert_eq!(std::fs::read(&outside).unwrap(), b"keep");

    // An unassign, a done move: the card dirs go.
    td.post(
        &format!("/api/cards/{c2}/assign"),
        json!({"assignee": null}),
    )
    .await
    .ok();
    wait_for("the unassigned card goes", || !cards_dir(&ws, &c2).exists()).await;
    td.post(&format!("/api/cards/{c1}/move"), json!({"column": "done"}))
        .await
        .ok();
    wait_for("the done card goes", || !cards_dir(&ws, &c1).exists()).await;
}

/// Wait until `p/w1` has a mail with this subject. The delivery writes the
/// files first and mails the notice after it (TER-188).
async fn wait_for_mail(td: &TestDaemon, subject: &str) {
    let start = std::time::Instant::now();
    loop {
        let mail = mail_subjects(td).await;
        if mail.iter().any(|s| s == subject) {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "timed out: the mail {subject:?} in {mail:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn mail_subjects(td: &TestDaemon) -> Vec<String> {
    td.mail_of("p/w1")
        .await
        .iter()
        .filter_map(|m| m["subject"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn delivery_refuses_a_symlinked_terrarium_dir() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let a = td.agent("p", "w1", json!({})).await;
    let id = a["id"].as_str().unwrap().to_owned();
    let ws = PathBuf::from(a["workspace"].as_str().unwrap());
    std::fs::create_dir_all(&ws).unwrap();
    let elsewhere = td.dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, ws.join(".terrarium")).unwrap();
    let c = card(&td, "p", None, "Linked").await;
    attach(&td, &c, None, "spec.md", b"x", false).await;
    // The assignment wakes the agent; its start tries the delivery.
    td.post(&format!("/api/cards/{c}/assign"), json!({"assignee": "w1"}))
        .await
        .ok();
    td.wait_status(&id, "idle").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
}
