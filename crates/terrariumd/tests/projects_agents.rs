//! Projects, agents, file sync, the fake supervisor, the PTY, audit, TCP.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::{TestDaemon, write};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn health_and_errors() {
    let td = TestDaemon::start().await;
    let h = td.get("/api/health").await;
    assert_eq!(h.ok()["status"], "ok");
    assert!(h.body["uptime"].is_number());
    td.get("/api/nope").await.err(404, "not_found");
    td.get("/api/hosts").await.ok();
    assert_eq!(td.get("/api/hosts").await.body, json!([]));
    // A bad body is a bad_request in the error shape.
    td.post("/api/projects", json!({"nom": "x"}))
        .await
        .err(400, "bad_request");
    // An unknown agent header is refused.
    td.get_as("nobody", "/api/projects")
        .await
        .err(401, "unauthorized");
}

#[tokio::test]
async fn project_crud_and_paging() {
    let td = TestDaemon::start().await;
    let p = td.project("alpha").await;
    assert_eq!(p["card_prefix"], "ALP");
    assert_eq!(p["source"], "api");
    td.post("/api/projects", json!({"name": "alpha"}))
        .await
        .err(409, "conflict");
    td.post("/api/projects", json!({"name": "Bad Name"}))
        .await
        .err(422, "invalid");
    // A clash of derived prefixes picks a longer one.
    let p2 = td.project("alpine").await;
    assert_eq!(p2["card_prefix"], "ALPI");
    let p3 = td
        .post(
            "/api/projects",
            json!({"name": "gamma", "card_prefix": "GG"}),
        )
        .await;
    assert_eq!(p3.ok()["card_prefix"], "GG");

    let page = td.get("/api/projects?limit=2").await;
    assert_eq!(page.ok().as_array().unwrap().len(), 2);
    let cursor = page.next_cursor().expect("a next cursor");
    let rest = td
        .get(&format!("/api/projects?limit=2&cursor={cursor}"))
        .await;
    assert_eq!(rest.ok()[0]["name"], "gamma");
    assert_eq!(rest.next_cursor(), None);

    let patched = td
        .patch("/api/projects/alpha", json!({"description": "first"}))
        .await;
    assert_eq!(patched.ok()["description"], "first");
    assert_eq!(
        td.get("/api/projects/alpha").await.ok()["description"],
        "first"
    );

    td.delete("/api/projects/gamma")
        .await
        .err(409, "confirm_required");
    assert_eq!(
        td.delete("/api/projects/gamma?confirm=true").await.status,
        204
    );
    td.get("/api/projects/gamma").await.err(404, "not_found");

    // Repos as data.
    let r = td
        .post(
            "/api/projects/alpha/repos",
            json!({"url": "git@github.com:wiltaylor/terrarium.git", "default_branch": "main"}),
        )
        .await;
    assert_eq!(r.ok()["name"], "terrarium");
    assert_eq!(r.body["path"], "terrarium");
    let repos = td.get("/api/projects/alpha/repos").await;
    assert_eq!(repos.ok().as_array().unwrap().len(), 1);
    td.post(
        "/api/projects/alpha/repos",
        json!({"url": "https://x/terrarium.git"}),
    )
    .await
    .err(409, "conflict");
}

const PROJECT: &str = r#"
description = "From a file"
prefix = "TER"

[[repos]]
name = "terrarium"
url = "git@github.com:wiltaylor/terrarium.git"
default_branch = "main"
"#;

const LEAD: &str = r#"
name = "lead"
project = "terra"
harness = "claude"
prompt = "project-lead"
roles = ["project_admin"]
"#;

const WORKER: &str = r#"
project = "terra"
harness = "pi"
model = "a/b"
"#;

const TARGET: &str = r#"
kind = "ssh"
project = "terra"
address = "build@host"
"#;

#[tokio::test]
async fn file_agents_sync_at_start_and_reload() {
    let mut td = TestDaemon::with_config(|c| {
        write(c, "projects/terra.toml", PROJECT);
        write(c, "agents/lead.toml", LEAD);
        write(c, "agents/worker.toml", WORKER);
        write(c, "targets/buildbox.toml", TARGET);
        write(
            c,
            "targets/global.toml",
            "kind = \"host\"\nhost = \"lab1\"\n",
        );
    })
    .await;
    let p = td.get("/api/projects/terra").await;
    assert_eq!(p.ok()["source"], "file");
    assert_eq!(p.body["card_prefix"], "TER");
    assert_eq!(
        td.get("/api/projects/terra/repos").await.ok()[0]["source"],
        "file"
    );
    let targets = td.get("/api/projects/terra/targets").await;
    let names: Vec<&str> = targets
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["buildbox", "global"]);
    assert_eq!(
        td.get("/api/targets").await.ok().as_array().unwrap().len(),
        2
    );

    let agents = td.get("/api/agents?project=terra").await;
    let agents = agents.ok().as_array().unwrap().clone();
    assert_eq!(agents.len(), 2);
    let lead = agents.iter().find(|a| a["name"] == "lead").unwrap();
    assert_eq!(lead["source"], "file");
    assert_eq!(lead["roles"], json!(["project_admin"]));
    let worker_id = agents.iter().find(|a| a["name"] == "worker").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // A file agent is read-only through the API.
    td.patch("/api/agents/terra%2Flead", json!({"model": "x"}))
        .await
        .err(409, "file_managed");
    td.put("/api/agents/terra%2Flead/roles", json!([]))
        .await
        .err(409, "file_managed");
    td.patch("/api/projects/terra", json!({"description": "x"}))
        .await
        .err(409, "file_managed");

    // Change the files and reload.
    std::fs::remove_file(td.config.join("agents/lead.toml")).unwrap();
    write(
        &td.config,
        "agents/worker.toml",
        &WORKER.replace("a/b", "c/d"),
    );
    write(
        &td.config,
        "agents/helper.toml",
        "project = \"terra\"\nharness = \"codex\"\n",
    );
    let report = td.post_empty("/api/reload").await;
    let report = report.ok();
    assert_eq!(report["agents"]["added"], json!(["terra/helper"]));
    assert_eq!(report["agents"]["updated"], json!(["terra/worker"]));
    assert_eq!(report["agents"]["removed"], json!(["terra/lead"]));
    td.get("/api/agents/terra%2Flead")
        .await
        .err(404, "not_found");
    let w = td.get(&format!("/api/agents/{worker_id}")).await;
    assert_eq!(w.ok()["model"], "c/d");

    // A restart keeps the identity of file agents.
    td.boot(None, None).await;
    let w = td.get("/api/agents/terra%2Fworker").await;
    assert_eq!(w.ok()["id"], worker_id.as_str());
    // Reload is for sysadmins and the operator only.
    td.post_empty_as("terra/worker", "/api/reload")
        .await
        .err(403, "forbidden");
}

#[tokio::test]
async fn agent_crud_and_roles() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.project("q").await;
    let a = td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await;
    assert_eq!(a["status"], "stopped");
    assert_eq!(a["source"], "api");
    assert!(a["workspace"].as_str().unwrap().ends_with("/workspace"));
    td.agent("p", "worker", json!({})).await;
    td.agent("q", "other", json!({})).await;
    td.post(
        "/api/agents",
        json!({"name": "lead", "project": "p", "harness": "claude"}),
    )
    .await
    .err(409, "conflict");
    td.post(
        "/api/agents",
        json!({"name": "x", "project": "nope", "harness": "claude"}),
    )
    .await
    .err(404, "not_found");

    // Members cannot manage agents; project admins can, in their project.
    td.post_as(
        "p/worker",
        "/api/agents",
        json!({"name": "w2", "project": "p", "harness": "pi"}),
    )
    .await
    .err(403, "forbidden");
    td.post_as(
        "p/lead",
        "/api/agents",
        json!({"name": "w2", "project": "p", "harness": "pi"}),
    )
    .await
    .ok();
    td.post_as(
        "p/lead",
        "/api/agents",
        json!({"name": "w3", "project": "q", "harness": "pi"}),
    )
    .await
    .err(403, "forbidden");

    // A member sees its own project only.
    let seen = td.get_as("p/worker", "/api/agents").await;
    assert_eq!(seen.ok().as_array().unwrap().len(), 3);
    td.get_as("p/worker", "/api/agents/q%2Fother")
        .await
        .err(403, "forbidden");
    td.get_as("p/worker", "/api/projects/q")
        .await
        .err(403, "forbidden");

    // Roles.
    assert_eq!(
        td.put_as(
            "p/lead",
            "/api/agents/p%2Fworker/roles",
            json!(["project_admin"])
        )
        .await
        .ok(),
        &json!(["project_admin"])
    );
    td.put_as(
        "p/lead",
        "/api/agents/p%2Fworker/roles",
        json!(["sysadmin"]),
    )
    .await
    .err(403, "forbidden");
    td.put_as("p/lead", "/api/agents/p%2Flead/roles", json!([]))
        .await
        .err(403, "forbidden");
    td.put(
        "/api/agents/q%2Fother/roles",
        json!({"roles": ["sysadmin"]}),
    )
    .await
    .ok();
    assert_eq!(
        td.get("/api/agents/q%2Fother/roles").await.ok(),
        &json!(["sysadmin"])
    );
    // A sysadmin reads every project.
    td.get_as("q/other", "/api/projects/p").await.ok();

    // Patch and delete.
    let patched = td
        .patch(
            "/api/agents/p%2Fw2",
            json!({"model": "m", "policy": {"mail_to": ["lead"], "targets": []}}),
        )
        .await;
    assert_eq!(patched.ok()["model"], "m");
    assert_eq!(patched.body["policy"]["mail_to"], json!(["lead"]));
    td.req(
        hyper::Method::DELETE,
        "/api/agents/p%2Flead",
        None,
        Some("p/lead"),
    )
    .await
    .err(403, "forbidden");
    assert_eq!(td.delete("/api/agents/p%2Fw2").await.status, 204);

    // Every privileged action is in the audit log.
    let audit = td.get("/api/audit?limit=1000").await;
    let actions: Vec<String> = audit
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["action"].as_str().unwrap().to_owned())
        .collect();
    for want in [
        "project.create",
        "agent.create",
        "agent.roles",
        "agent.update",
        "agent.delete",
    ] {
        assert!(actions.iter().any(|a| a == want), "{want} in {actions:?}");
    }
    td.get_as("p/worker", "/api/audit").await.ok(); // worker is project_admin now
    let mine = td.get_as("p/lead", "/api/audit?limit=1000").await;
    assert!(
        mine.ok()
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["project"] == "p")
    );
}

fn statuses(events: &Value) -> Vec<String> {
    events
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "status")
        .map(|e| e["data"]["status"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn fake_supervisor_status_flow() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let a = td.agent("p", "a", json!({})).await;
    let id = a["id"].as_str().unwrap().to_owned();

    td.post(&format!("/api/agents/{id}/send"), json!({"text": "hi"}))
        .await
        .err(409, "not_running");
    let started = td.post_empty(&format!("/api/agents/{id}/start")).await;
    assert_eq!(started.status, 202);
    td.wait_status(&id, "idle").await;
    td.post_empty(&format!("/api/agents/{id}/start"))
        .await
        .err(409, "already_running");

    td.post(&format!("/api/agents/{id}/send"), json!({"text": "do it"}))
        .await
        .ok();
    td.wait_status(&id, "working").await;
    td.wait_status(&id, "idle").await;
    td.post(&format!("/api/agents/{id}/answer"), json!({"text": "y"}))
        .await
        .ok();
    td.post(
        &format!("/api/agents/{id}/answer"),
        json!({"request_id": "r1", "answer": {"kind": "choice", "value": 0}}),
    )
    .await
    .ok();
    td.post(
        &format!("/api/agents/{id}/answer"),
        json!({"request_id": "r1"}),
    )
    .await
    .err(400, "bad_request");
    td.wait_status(&id, "idle").await;

    let events = td.get(&format!("/api/agents/{id}/events?since=0")).await;
    let all = events.ok();
    let s = statuses(all);
    assert_eq!(&s[..4], ["starting", "idle", "working", "idle"]);
    let kinds: Vec<&str> = all
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"session_started"));
    assert!(kinds.contains(&"turn_start"));
    assert!(kinds.contains(&"turn_end"));
    let first = &all[0];
    assert_eq!(first["agent"], "a");
    assert_eq!(first["data"]["agent_id"], id.as_str());
    assert!(first["summary"].as_str().unwrap().contains("starting"));
    // ?since= skips what the client has.
    let seq = all[1]["seq"].as_i64().unwrap();
    let later = td
        .get(&format!("/api/agents/{id}/events?since={seq}"))
        .await;
    assert_eq!(later.ok()[0]["seq"].as_i64().unwrap(), seq + 1);
    let by_id = td
        .get(&format!(
            "/api/agents/{id}/events?since={}",
            all[1]["id"].as_str().unwrap()
        ))
        .await;
    assert_eq!(by_id.ok()[0]["seq"].as_i64().unwrap(), seq + 1);

    let agent = td.get(&format!("/api/agents/{id}")).await;
    assert!(agent.ok()["session"].as_str().unwrap().starts_with("fake-"));

    td.post_empty(&format!("/api/agents/{id}/restart"))
        .await
        .ok();
    td.wait_status(&id, "idle").await;
    td.post_empty(&format!("/api/agents/{id}/stop")).await.ok();
    td.wait_status(&id, "stopped").await;
    td.post_empty(&format!("/api/agents/{id}/stop"))
        .await
        .err(409, "not_running");

    let sessions = td.get(&format!("/api/agents/{id}/sessions")).await;
    let sessions = sessions.ok().as_array().unwrap().clone();
    assert_eq!(sessions.len(), 2);
    // Newest first.
    assert_eq!(sessions[0]["outcome"], "stopped");
    assert_eq!(sessions[0]["current"], false);
    assert_eq!(sessions[1]["outcome"], "restarted");
    assert_eq!(sessions[1]["turns"], 3);
    // The restart resumed the same harness session.
    assert_eq!(
        sessions[0]["harness_session_id"],
        sessions[1]["harness_session_id"]
    );

    // A live agent needs ?confirm=true to delete.
    td.post_empty(&format!("/api/agents/{id}/start")).await.ok();
    td.wait_status(&id, "idle").await;
    td.delete(&format!("/api/agents/{id}"))
        .await
        .err(409, "confirm_required");
    assert_eq!(
        td.delete(&format!("/api/agents/{id}?confirm=true"))
            .await
            .status,
        204
    );
    // The log outlives the agent.
    let fire = td.get(&format!("/api/events?agent={id}&kind=status")).await;
    assert!(fire.ok().as_array().unwrap().len() >= 8);
    td.get("/api/events?agent=p%2Fgone")
        .await
        .err(404, "not_found");
}

#[tokio::test]
async fn events_websocket_and_sse() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let a = td.agent("p", "a", json!({})).await;
    let id = a["id"].as_str().unwrap().to_owned();

    let mut ws = td
        .ws(&format!("/api/agents/{id}/events?since=0"), None)
        .await
        .unwrap();
    let mut fire = td.ws("/api/events?kind=status", None).await.unwrap();
    td.post_empty(&format!("/api/agents/{id}/start")).await.ok();

    let mut seen = Vec::new();
    while seen.len() < 2 {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let ev: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        if ev["kind"] == "status" {
            seen.push(ev["data"]["status"].as_str().unwrap().to_owned());
        }
    }
    assert_eq!(seen, ["starting", "idle"]);
    let msg = tokio::time::timeout(Duration::from_secs(5), fire.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let ev: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(ev["kind"], "status");
    assert_eq!(ev["agent"], "a");

    // A reconnect with ?since= gets the backlog.
    let last = 1;
    let mut again = td
        .ws(&format!("/api/agents/{id}/events?since={last}"), None)
        .await
        .unwrap();
    let msg = tokio::time::timeout(Duration::from_secs(5), again.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let ev: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert!(ev["seq"].as_i64().unwrap() > last);

    // SSE.
    let stream = tokio::net::UnixStream::connect(&td.socket).await.unwrap();
    let (mut sender, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .unwrap();
    tokio::spawn(conn);
    let req = hyper::Request::builder()
        .uri(format!("/api/agents/{id}/events?since=0"))
        .header("host", "localhost")
        .header("accept", "text/event-stream")
        .body(http_body_util::Empty::<hyper::body::Bytes>::new())
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    assert_eq!(
        resp.headers()["content-type"].to_str().unwrap(),
        "text/event-stream"
    );
    let mut body = resp.into_body();
    let frame = tokio::time::timeout(
        Duration::from_secs(5),
        http_body_util::BodyExt::frame(&mut body),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    let text = String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap();
    assert!(text.contains("event: status"), "{text}");
    assert!(text.contains("id: 1"), "{text}");
}

#[tokio::test]
async fn pty_websocket_echoes_and_resizes() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let a = td.agent("p", "a", json!({})).await;
    td.agent("p", "member", json!({})).await;
    let id = a["id"].as_str().unwrap().to_owned();

    // Not running: the upgrade is refused with not_running.
    let err = td
        .ws(&format!("/api/agents/{id}/pty"), None)
        .await
        .unwrap_err();
    match err {
        tokio_tungstenite::tungstenite::Error::Http(resp) => assert_eq!(resp.status(), 409),
        other => panic!("unexpected {other:?}"),
    }
    // A plain GET is a bad request.
    td.get(&format!("/api/agents/{id}/pty"))
        .await
        .err(400, "bad_request");

    td.post_empty(&format!("/api/agents/{id}/start")).await.ok();
    td.wait_status(&id, "idle").await;
    // Members may not attach.
    assert!(
        td.ws(&format!("/api/agents/{id}/pty"), Some("p/member"))
            .await
            .is_err()
    );
    let mut ws = td.ws(&format!("/api/agents/{id}/pty"), None).await.unwrap();
    let snapshot = ws.next().await.unwrap().unwrap();
    assert!(matches!(snapshot, Message::Binary(_)));
    ws.send(Message::text(
        json!({"type": "resize", "rows": 30, "cols": 100}).to_string(),
    ))
    .await
    .unwrap();
    ws.send(Message::text(
        json!({"type": "input", "data": "hello"}).to_string(),
    ))
    .await
    .unwrap();
    let echo = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(echo, Message::Binary(b"hello".to_vec().into()));
    ws.send(Message::Binary(b"raw".to_vec().into()))
        .await
        .unwrap();
    let echo = ws.next().await.unwrap().unwrap();
    assert_eq!(echo, Message::Binary(b"raw".to_vec().into()));
    assert_eq!(td.sup.pty_size(id.parse().unwrap()), Some((30, 100)));

    // Stop: the socket closes.
    td.post_empty(&format!("/api/agents/{id}/stop")).await.ok();
    let end = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .unwrap();
    assert!(matches!(
        end,
        None | Some(Ok(Message::Close(_))) | Some(Err(_))
    ));
}

#[tokio::test]
async fn tcp_needs_the_token_and_ignores_the_agent_header() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut td = TestDaemon::start().await;
    td.project("p").await;
    td.agent("p", "member", json!({})).await;
    td.boot(Some("127.0.0.1:0"), Some("sekret")).await;
    let addr = td.daemon.as_ref().unwrap().tcp_addr.unwrap();

    let get = |path: &'static str, token: Option<&'static str>, agent: bool| async move {
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut sender, conn) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
                .await
                .unwrap();
        tokio::spawn(conn);
        let mut req = hyper::Request::builder()
            .uri(path)
            .header("host", "localhost");
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        if agent {
            req = req.header("x-terrarium-agent", "p/member");
        }
        sender
            .send_request(
                req.body(http_body_util::Empty::<hyper::body::Bytes>::new())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    };
    assert_eq!(get("/api/health", None, false).await, 401);
    assert_eq!(get("/api/health", Some("sekret"), false).await, 200);
    // Over TCP the header is ignored: the caller is the operator, who may
    // read the audit log; the member could not.
    assert_eq!(get("/api/audit", Some("sekret"), true).await, 200);

    // A WebSocket upgrade with `Authorization` (the CLI's `agent attach`).
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut req = format!("ws://{addr}/api/events")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", "Bearer sekret".parse().unwrap());
    assert!(tokio_tungstenite::client_async(req, stream).await.is_ok());
    // `?token=` is off by default (TERRARIUM_WS_QUERY_TOKEN=0).
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let off =
        tokio_tungstenite::client_async(format!("ws://{addr}/api/events?token=sekret"), stream)
            .await;
    assert!(off.is_err());

    // TERRARIUM_WS_QUERY_TOKEN=1: upgrades may carry ?token=.
    td.web.ws_query_token = true;
    td.boot(Some("127.0.0.1:0"), Some("sekret")).await;
    let addr = td.daemon.as_ref().unwrap().tcp_addr.unwrap();
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let ok =
        tokio_tungstenite::client_async(format!("ws://{addr}/api/events?token=sekret"), stream)
            .await;
    assert!(ok.is_ok());
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let bad =
        tokio_tungstenite::client_async(format!("ws://{addr}/api/events?token=no"), stream).await;
    assert!(bad.is_err());
}

#[tokio::test]
async fn patch_clears_optional_fields_with_null() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.agent(
        "p",
        "w",
        json!({"subscription": "s1", "model": "haiku", "prompt": "custom"}),
    )
    .await;
    // An absent field stays; a value sets it.
    let v = td
        .patch("/api/agents/p%2Fw", json!({"model": "sonnet"}))
        .await;
    let v = v.ok();
    assert_eq!(v["model"], "sonnet");
    assert_eq!(v["subscription"], "s1");
    assert_eq!(v["prompt"], "custom");
    // `null` clears it.
    let v = td
        .patch(
            "/api/agents/p%2Fw",
            json!({"subscription": null, "model": null, "prompt": null}),
        )
        .await;
    let v = v.ok();
    for k in ["subscription", "model", "prompt"] {
        assert!(v[k].is_null(), "{k}: {v}");
    }
}

#[tokio::test]
async fn events_live_and_latest_skip_the_backlog() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let a = td.agent("p", "a", json!({})).await;
    let id = a["id"].as_str().unwrap().to_owned();
    td.post_empty(&format!("/api/agents/{id}/start")).await.ok();
    td.wait_status(&id, "idle").await;

    let latest = td.get("/api/events/latest").await.ok()["seq"]
        .as_i64()
        .unwrap();
    assert!(latest > 0);
    // JSON: no backlog after `latest`.
    let page = td.get("/api/events?since=latest").await;
    assert_eq!(page.ok(), &json!([]));
    let page = td.get("/api/events?live=1").await;
    assert_eq!(page.ok(), &json!([]));
    td.get("/api/events?live=maybe")
        .await
        .err(400, "bad_request");
    // `?since=<seq>` keeps its meaning.
    let page = td.get(&format!("/api/events?since={}", latest - 1)).await;
    assert_eq!(page.ok().as_array().unwrap().len(), 1);

    // WebSocket: the first frame is a new event, not the backlog.
    for path in ["/api/events?live=1", "/api/events?since=latest"] {
        let mut ws = td.ws(path, None).await.unwrap();
        td.post_empty(&format!("/api/agents/{id}/stop")).await.ok();
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let ev: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        assert!(ev["seq"].as_i64().unwrap() > latest, "{path}: {ev}");
        td.wait_status(&id, "stopped").await;
        td.post_empty(&format!("/api/agents/{id}/start")).await.ok();
        td.wait_status(&id, "idle").await;
    }
}
