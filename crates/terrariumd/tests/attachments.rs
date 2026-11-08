//! `POST /api/agents/{id}/attachments` (docs/api.md §6.5): an image into
//! the agent's workspace, the path that the agent sees, the magic-byte
//! check, the size limit, the rights of `send`, and the git exclude.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::{Resp, TestDaemon};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::UnixStream;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDRfake-png-body";

async fn upload(
    td: &TestDaemon,
    agent: &str,
    as_agent: Option<&str>,
    ct: &str,
    body: Vec<u8>,
) -> Resp {
    let stream = UnixStream::connect(&td.socket).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    let mut req = Request::builder()
        .method(Method::POST)
        .uri(format!("/api/agents/{agent}/attachments"))
        .header("host", "localhost")
        .header("content-type", ct);
    if let Some(a) = as_agent {
        req = req.header("x-terrarium-agent", a);
    }
    let resp = sender
        .send_request(req.body(Full::new(Bytes::from(body))).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let raw = resp.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&raw)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&raw).into()));
    Resp {
        status,
        headers,
        body,
    }
}

#[tokio::test]
async fn upload_puts_the_image_in_the_workspace() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.project("q").await;
    let a = td.agent("p", "w1", json!({})).await;
    let id = a["id"].as_str().unwrap();
    let workspace = std::path::PathBuf::from(a["workspace"].as_str().unwrap());
    // The workspace root is a git repo: the exclude line goes in once.
    std::fs::create_dir_all(workspace.join(".git/info")).unwrap();
    std::fs::write(workspace.join(".git/info/exclude"), "# git ls-files\n*.log").unwrap();

    let r = upload(&td, "p%2Fw1", None, "image/png", PNG.to_vec()).await;
    assert_eq!(r.status.as_u16(), 201, "{}", r.body);
    let b = r.ok();
    let name = b["name"].as_str().unwrap();
    assert!(
        name.ends_with(".png") && name.len() == "20261028-120000-abcdefgh.png".len(),
        "{b}"
    );
    assert_eq!(
        b["path"],
        format!("/work/p/w1/.terrarium/attachments/{name}"),
        "the path the sandbox shows"
    );
    assert_eq!(b["bytes"], PNG.len());
    assert_eq!(b["content_type"], "image/png");
    assert!(b.get("host_path").is_none());
    let on_host = workspace.join(".terrarium/attachments").join(name);
    assert_eq!(std::fs::read(&on_host).unwrap(), PNG);
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&on_host).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
    }

    // The header says JPEG, the bytes say PNG: the bytes win.
    let r = upload(&td, id, None, "image/jpeg", PNG.to_vec()).await;
    assert_eq!(r.ok()["content_type"], "image/png");
    let exclude = std::fs::read_to_string(workspace.join(".git/info/exclude")).unwrap();
    assert_eq!(exclude.matches("/.terrarium/").count(), 1, "{exclude}");
    assert!(exclude.starts_with("# git ls-files\n*.log\n"), "{exclude}");

    // The JSON form (the CLI): the same file.
    let b64 = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(PNG)
    };
    let r = upload(
        &td,
        id,
        None,
        "application/json",
        json!({"data_base64": b64}).to_string().into_bytes(),
    )
    .await;
    assert_eq!(r.status.as_u16(), 201, "{}", r.body);
    let n = r.body["name"].as_str().unwrap();
    assert_eq!(
        std::fs::read(workspace.join(".terrarium/attachments").join(n)).unwrap(),
        PNG
    );

    // The audit row has the name and the size, not the bytes.
    let audit = td.get("/api/audit?action=agent.attachment").await;
    let rows = audit.ok().as_array().unwrap().clone();
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(rows[0]["detail"]["bytes"], PNG.len());
    assert!(rows[0]["detail"]["name"].is_string());

    // Not an image by its bytes, or not an image type: 415.
    upload(&td, id, None, "image/png", b"<svg xmlns='x'/>".to_vec())
        .await
        .err(415, "unsupported_media_type");
    upload(&td, id, None, "text/plain", PNG.to_vec())
        .await
        .err(415, "unsupported_media_type");
    // Over 20 MiB: 413.
    let mut big = PNG.to_vec();
    big.resize(terrariumd::api::attachments::MAX_ATTACHMENT_BYTES + 1, 0);
    upload(&td, id, None, "image/png", big)
        .await
        .err(413, "too_large");

    // A member of another project, and a plain member: 403. The own lead: 201.
    let other = td
        .agent("q", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let member = td.agent("p", "m", json!({})).await;
    let lead = td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await;
    for who in [&other, &member] {
        let r = upload(
            &td,
            id,
            Some(who["id"].as_str().unwrap()),
            "image/png",
            PNG.to_vec(),
        )
        .await;
        assert!(matches!(r.status.as_u16(), 403 | 404), "{}", r.body);
    }
    upload(
        &td,
        id,
        Some(member["id"].as_str().unwrap()),
        "image/png",
        PNG.to_vec(),
    )
    .await
    .err(403, "forbidden");
    let r = upload(
        &td,
        id,
        Some(lead["id"].as_str().unwrap()),
        "image/png",
        PNG.to_vec(),
    )
    .await;
    assert_eq!(r.status.as_u16(), 201, "{}", r.body);
    let files = std::fs::read_dir(workspace.join(".terrarium/attachments"))
        .unwrap()
        .count();
    assert_eq!(
        files, 4,
        "three by the operator, one by the lead; no temp files"
    );
}
