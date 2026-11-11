//! TER-62: two daemons and hostile upstreams, without external network access.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common;

use axum::body::{Body, Bytes};
use axum::extract::FromRequestParts;
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, routing::get};
use common::TestDaemon;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use terrariumd::remotes as net;

async fn remote() -> TestDaemon {
    let mut d = TestDaemon::start().await;
    d.boot(Some("127.0.0.1:0"), Some("test-remote-operator"))
        .await;
    d.project("terra").await;
    d
}

#[tokio::test]
async fn artifact_capability_serves_without_credentials_and_ends_on_logout() {
    use base64::Engine;
    let mut home = TestDaemon::start().await;
    home.boot(Some("127.0.0.1:0"), Some("test-home-operator"))
        .await;
    let remote = remote().await;
    link(&home, &remote, false).await;
    let artifact = remote.post("/api/projects/terra/artifacts", json!({
        "name":"report.html", "content_type":"text/html",
        "data_base64":base64::engine::general_purpose::STANDARD.encode(b"<!doctype html><p>remote report</p>")
    })).await.ok().clone();
    let id = artifact["uid"].as_str().unwrap();
    let base = format!("http://{}", home.daemon.as_ref().unwrap().tcp_addr.unwrap());
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let mint = format!("{base}/api/remotes/lab/artifacts/{id}/url");
    // A static operator token does not stand in for a session binding.
    assert_eq!(
        client
            .post(&mint)
            .bearer_auth("test-home-operator")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let login = client
        .post(format!("{base}/api/session"))
        .bearer_auth("test-home-operator")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);
    let cookie = login.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let login: Value = login.json().await.unwrap();
    let csrf = login["csrf"].as_str().unwrap();
    let signed = client
        .post(&mint)
        .header(header::COOKIE, &cookie)
        .header("x-csrf-token", csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(
        signed.status(),
        StatusCode::OK,
        "{}",
        signed.text().await.unwrap()
    );
    let signed: Value = signed.json().await.unwrap();
    let url = signed["url"].as_str().unwrap();
    let url = if url.starts_with('/') {
        format!("{base}{url}")
    } else {
        url.to_owned()
    };
    // An unauthenticated browser on the artifact origin needs only this URL.
    let response = client.get(&url).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert!(
        response.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("sandbox")
    );
    assert!(!response.headers().contains_key(header::SET_COOKIE));
    assert!(response.text().await.unwrap().contains("remote report"));
    // Neither an invalid signature nor a suffix for another file is accepted.
    let mut bad = url.clone();
    let last = bad.len() - 2;
    bad.replace_range(last..last + 1, "z");
    assert_eq!(
        client.get(&bad).send().await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        client
            .get(format!("{url}other.txt"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let logout = client
        .delete(format!("{base}/api/session"))
        .header(header::COOKIE, cookie)
        .header("x-csrf-token", csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(logout.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        client.get(&url).send().await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn browser_admission_is_retained_until_socket_close_and_has_retry_after() {
    let home = TestDaemon::start().await;
    let remote = remote().await;
    link(&home, &remote, false).await;
    let mut tabs = Vec::new();
    for _ in 0..32 {
        tabs.push(
            home.ws("/api/remotes/lab/proxy/api/events?live=1", None)
                .await
                .unwrap(),
        );
    }
    let error = home
        .ws("/api/remotes/lab/proxy/api/events?live=1", None)
        .await
        .expect_err("33rd browser must be refused");
    let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
        panic!("expected HTTP refusal");
    };
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()["retry-after"], "1");
    home.delete("/api/remotes/lab").await.ok();
    drop(tabs);
}

#[tokio::test]
async fn historical_pages_obey_upstream_request_admission() {
    let home = TestDaemon::start().await;
    let id = ulid::Ulid::new().to_string();
    let instance = id.clone();
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Semaphore::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (a, m, e, g) = (
        active.clone(),
        maximum.clone(),
        entered.clone(),
        release.clone(),
    );
    let app = Router::new()
        .route(
            "/api/system/instance",
            get(move || {
                let id = instance.clone();
                async move { Json(json!({"id":id,"version":terrarium_version::VERSION})) }
            }),
        )
        .route(
            "/api/events/latest",
            get(|| async { Json(json!({"seq":0})) }),
        )
        .route(
            "/api/events",
            get(move |req: axum::extract::Request| {
                let (a, m, e, g) = (a.clone(), m.clone(), e.clone(), g.clone());
                async move {
                    if req.headers().contains_key("upgrade") {
                        let (mut parts, _) = req.into_parts();
                        let ws =
                            axum::extract::WebSocketUpgrade::from_request_parts(&mut parts, &())
                                .await
                                .unwrap();
                        return ws
                            .on_upgrade(|socket| async move {
                                let _socket = socket;
                                std::future::pending::<()>().await;
                            })
                            .into_response();
                    }
                    let n = a.fetch_add(1, Ordering::SeqCst) + 1;
                    m.fetch_max(n, Ordering::SeqCst);
                    e.add_permits(1);
                    g.acquire().await.unwrap().forget();
                    a.fetch_sub(1, Ordering::SeqCst);
                    Json(json!([])).into_response()
                }
            }),
        );
    let _server = fake(&home, app, &id).await;
    let live = home
        .ws("/api/remotes/lab/proxy/api/events?live=1", None)
        .await
        .unwrap();
    let row = home.get("/api/remotes/lab").await;
    let limits = home
        .daemon
        .as_ref()
        .unwrap()
        .state
        .remotes
        .get(row.ok()["id"].as_str().unwrap());
    let held: Vec<_> = (0..62).map(|_| limits.acquire(false).unwrap()).collect();
    let first = home
        .ws("/api/remotes/lab/proxy/api/events?since=0", None)
        .await
        .unwrap();
    let second = home
        .ws("/api/remotes/lab/proxy/api/events?since=0", None)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered.acquire_many(2))
        .await
        .unwrap()
        .unwrap()
        .forget();
    home.get("/api/remotes/lab/proxy/api/projects")
        .await
        .err(429, "rate_limited");
    assert_eq!(maximum.load(Ordering::SeqCst), 2);
    assert!(limits.acquire(false).is_err());
    release.add_permits(2);
    drop(held);
    home.delete("/api/remotes/lab").await.ok();
    drop((live, first, second));
}

#[tokio::test]
async fn remote_count_is_capped_lists_are_paged_and_duplicate_checks_use_index() {
    let home = TestDaemon::start().await;
    let remote = remote().await;
    link(&home, &remote, false).await;
    let key = terrariumd::secrets::key(&home.state).unwrap();
    home.daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .write("operator", move |t| {
            let base = terrariumd::store::remotes::get(t, "lab")?;
            for i in 0..63 {
                let mut row = base.clone();
                row.id = ulid::Ulid::new().to_string();
                row.name = format!("remote{i:02}");
                let sealed = key.seal(b"test-key", &terrariumd::store::remotes::aad(&row.id))?;
                terrariumd::store::remotes::insert(t, &row, &sealed)?;
            }
            let mut overflow = base.clone();
            overflow.id = ulid::Ulid::new().to_string();
            overflow.name = "overflow".into();
            assert!(terrariumd::store::remotes::insert(t, &overflow, b"unused").is_err());
            assert_eq!(
                terrariumd::store::remotes::list_page(t, None, usize::MAX)?.len(),
                64
            );
            let sql = format!(
                "EXPLAIN QUERY PLAN {}",
                terrariumd::store::remotes::EXISTS_SQL
            );
            let plan = t
                .prepare(&sql)?
                .query_map(["lab"], |r| r.get::<_, String>(3))?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .join(" ");
            assert!(
                plan.contains("SEARCH remotes") && plan.contains("INDEX"),
                "{plan}"
            );
            assert!(terrariumd::store::remotes::check_add(t, "lab").is_err());
            Ok(())
        })
        .await
        .unwrap();
    let refusal = home.post("/api/remotes",json!({"name":"overflow","url":"http://127.0.0.1:1","instance_id":ulid::Ulid::new().to_string(),"code":"unused"})).await;
    refusal.err(409, "conflict");
    assert!(
        refusal.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("64")
    );
    let page = home.get("/api/remotes?limit=2").await;
    assert_eq!(page.ok().as_array().unwrap().len(), 2);
    let cursor = page.next_cursor().unwrap();
    let next = home
        .get(&format!("/api/remotes?limit=2&cursor={cursor}"))
        .await;
    assert_eq!(next.ok().as_array().unwrap().len(), 2);
    assert_ne!(page.ok()[0]["name"], next.ok()[0]["name"]);
}

#[tokio::test]
async fn agent_name_filters_match_backlog_and_live_with_independent_tabs() {
    let home = TestDaemon::start().await;
    let remote = remote().await;
    let worker = remote.agent("terra", "worker", json!({})).await;
    let other = remote.agent("terra", "other", json!({})).await;
    let (worker, other) = (
        worker["id"].as_str().unwrap().to_owned(),
        other["id"].as_str().unwrap().to_owned(),
    );
    async fn emit(d: &TestDaemon, worker: &str, other: &str) {
        let (worker, other) = (worker.to_owned(), other.to_owned());
        d.daemon
            .as_ref()
            .unwrap()
            .state
            .db
            .write("operator", move |t| {
                for (id, name) in [(worker, "worker"), (other, "other")] {
                    t.emit(
                        terrariumd::events::EventDraft::new("filter-test", "test event", json!({}))
                            .agent(id, name)
                            .project("terra"),
                    )?;
                }
                Ok(())
            })
            .await
            .unwrap();
    }
    async fn received(
        ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::UnixStream>,
        id: &str,
    ) {
        let frame = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let v: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
        assert_eq!(v["agent_id"], id);
    }
    emit(&remote, &worker, &other).await;
    link(&home, &remote, false).await;
    let mut first = home
        .ws(
            "/api/remotes/lab/proxy/api/events?since=0&agent=worker&kind=filter-test",
            None,
        )
        .await
        .unwrap();
    let mut second = home
        .ws(
            "/api/remotes/lab/proxy/api/events?since=0&agent=terra%2Fother&kind=filter-test",
            None,
        )
        .await
        .unwrap();
    received(&mut first, &worker).await;
    received(&mut second, &other).await;
    emit(&remote, &worker, &other).await;
    received(&mut first, &worker).await;
    received(&mut second, &other).await;
    home.delete("/api/remotes/lab").await.ok();
}

async fn link(home: &TestDaemon, remote: &TestDaemon, readonly: bool) -> Value {
    let p = remote
        .post(
            "/api/links",
            json!({"label": "home", "read_only": readonly}),
        )
        .await;
    let addr = remote.daemon.as_ref().unwrap().tcp_addr.unwrap();
    home.post(
        "/api/remotes",
        json!({"name": "lab", "url": format!("http://{addr}"),
        "instance_id": p.ok()["instance"]["id"], "code": p.ok()["code"]}),
    )
    .await
    .ok()
    .clone()
}

#[tokio::test]
async fn two_daemons_pair_proxy_revoke_and_remove() {
    let home = TestDaemon::start().await;
    let remote = remote().await;
    let r = link(&home, &remote, false).await;
    assert!(r.get("key").is_none() && r.get("sealed").is_none());
    let sys = remote
        .agent("terra", "sys", json!({"roles":["sysadmin"]}))
        .await;
    let id = sys["id"].as_str().unwrap();
    home.post(
        &format!("/api/remotes/lab/proxy/api/agents/{id}/send"),
        json!({"text":"do not deliver"}),
    )
    .await
    .err(403, "link_forbidden");
    home.post(
        "/api/remotes/lab/proxy/api/mail",
        json!({"to":id,"body":"do not deliver"}),
    )
    .await
    .err(403, "link_forbidden");
    home.post(
        "/api/remotes/lab/proxy/api/projects",
        json!({"name":"remote-project"}),
    )
    .await
    .ok();
    assert!(
        remote
            .get("/api/projects/remote-project")
            .await
            .status
            .is_success()
    );
    assert_eq!(
        home.get("/api/projects/remote-project").await.status,
        StatusCode::NOT_FOUND
    );
    let listed = home.get("/api/remotes").await;
    assert_eq!(listed.ok()[0]["status"], "online");
    let links = remote.get("/api/links").await;
    let id = links.ok()[0]["id"].as_str().unwrap();
    remote.delete(&format!("/api/links/{id}")).await.ok();
    home.get("/api/remotes/lab/proxy/api/projects")
        .await
        .err(401, "unauthorized");
    assert_eq!(
        home.get("/api/remotes").await.ok()[0]["status"],
        "auth_failed"
    );
    let result = home.delete("/api/remotes/lab").await;
    assert_eq!(result.ok()["revoke_on_remote"], true);
    assert!(
        home.get("/api/remotes")
            .await
            .ok()
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn ten_browser_tabs_share_one_remote_firehose_and_keep_filters() {
    let home = TestDaemon::start().await;
    let id = ulid::Ulid::new().to_string();
    let instance = id.clone();
    let connects = Arc::new(AtomicUsize::new(0));
    let counter = connects.clone();
    let (tx, _) = tokio::sync::broadcast::channel::<String>(8);
    let upstream = tx.clone();
    let app = Router::new()
        .route(
            "/api/system/instance",
            get(move || {
                let id = instance.clone();
                async move { Json(json!({"id":id,"version":terrarium_version::VERSION})) }
            }),
        )
        .route(
            "/api/events/latest",
            get(|| async { Json(json!({"seq":0})) }),
        )
        .route(
            "/api/events",
            get(move |ws: axum::extract::WebSocketUpgrade| {
                counter.fetch_add(1, Ordering::SeqCst);
                let mut rx = upstream.subscribe();
                async move {
                    ws.on_upgrade(move |mut socket| async move {
                        while let Ok(text) = rx.recv().await {
                            if socket
                                .send(axum::extract::ws::Message::Text(text.into()))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    })
                }
            }),
        );
    let _server = fake(&home, app, &id).await;
    let mut tabs = Vec::new();
    for _ in 0..10 {
        tabs.push(
            home.ws(
                "/api/remotes/lab/proxy/api/events?live=1&project=terra",
                None,
            )
            .await
            .unwrap(),
        );
    }
    assert_eq!(connects.load(Ordering::SeqCst), 1);
    tx.send(json!({"seq":1,"kind":"test","project":"other"}).to_string())
        .unwrap();
    tx.send(json!({"seq":2,"kind":"test","project":"terra"}).to_string())
        .unwrap();
    for tab in &mut tabs {
        let frame = tokio::time::timeout(Duration::from_secs(10), tab.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let v: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
        assert_eq!(v["seq"], 2);
    }
    assert_eq!(connects.load(Ordering::SeqCst), 1);
    home.delete("/api/remotes/lab").await.ok();
}

#[tokio::test]
async fn firehose_reconnect_releases_its_stream_slot_and_resumes_sequence() {
    let home = TestDaemon::start().await;
    let id = ulid::Ulid::new().to_string();
    let instance = id.clone();
    let connects = Arc::new(AtomicUsize::new(0));
    let counter = connects.clone();
    let app = Router::new()
        .route(
            "/api/system/instance",
            get(move || {
                let id = instance.clone();
                async move { Json(json!({"id":id,"version":terrarium_version::VERSION})) }
            }),
        )
        .route(
            "/api/events/latest",
            get(|| async { Json(json!({"seq":0})) }),
        )
        .route(
            "/api/events",
            get(
                move |ws: axum::extract::WebSocketUpgrade,
                      axum::extract::Query(q): axum::extract::Query<
                    std::collections::HashMap<String, String>,
                >| {
                    let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                    async move {
                        ws.on_upgrade(move |mut socket| async move {
                            // Report the resume cursor as data, without putting remote
                            // events into the home's event store.
                            let text =
                                json!({"seq":n,"kind":"test","resume":q.get("since")}).to_string();
                            socket
                                .send(axum::extract::ws::Message::Text(text.into()))
                                .await
                                .unwrap();
                            if n == 1 {
                                let _ = socket.send(axum::extract::ws::Message::Close(None)).await;
                            } else {
                                std::future::pending::<()>().await;
                            }
                        })
                    }
                },
            ),
        );
    let _server = fake(&home, app, &id).await;
    let r = home.get("/api/remotes/lab").await;
    let limits = home
        .daemon
        .as_ref()
        .unwrap()
        .state
        .remotes
        .get(r.ok()["id"].as_str().unwrap());
    let _other_streams: Vec<_> = (0..7).map(|_| limits.acquire(true).unwrap()).collect();
    let mut ws = home
        .ws("/api/remotes/lab/proxy/api/events?live=1", None)
        .await
        .unwrap();
    for (seq, resume) in [(1, "0"), (2, "1")] {
        let frame = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let value: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
        assert_eq!(value["seq"], seq);
        assert_eq!(value["resume"], resume);
    }
    assert_eq!(connects.load(Ordering::SeqCst), 2);
    home.delete("/api/remotes/lab").await.ok();
}

#[tokio::test]
async fn read_only_and_sysadmin_home_are_refused() {
    let home = TestDaemon::start().await;
    let remote = remote().await;
    link(&home, &remote, true).await;
    home.post("/api/remotes/lab/proxy/api/projects", json!({"name":"no"}))
        .await
        .err(403, "link_forbidden");
    home.get("/api/remotes/lab/proxy/api/projects").await.ok();
    home.project("terra").await;
    let a = home
        .agent("terra", "admin", json!({"roles":["sysadmin"]}))
        .await;
    let id = a["id"].as_str().unwrap();
    home.get_as(id, "/api/remotes").await.err(403, "forbidden");
    home.get_as(id, "/api/remotes/lab/proxy/api/projects")
        .await
        .err(403, "forbidden");
    home.post_as(id, "/api/remotes", json!({"name":"no"}))
        .await
        .err(403, "forbidden");
    // P1 intentionally refuses DELETE /links/self for read-only links.
    assert_eq!(home.delete("/api/remotes/lab").await.ok()["revoked"], false);
    assert_eq!(
        remote.get("/api/links?revoked=true").await.ok()[0]["status"],
        "active"
    );
}

#[tokio::test]
async fn json_body_cap_and_read_timeout_are_enforced() {
    let app = Router::new()
        .route("/body", get(|| async { "123456789" }))
        .route(
            "/slow",
            get(|| async {
                let first = futures_util::stream::iter([Ok::<Bytes, std::io::Error>(
                    Bytes::from_static(b"x"),
                )]);
                Response::new(Body::from_stream(
                    first.chain(futures_util::stream::pending()),
                ))
            }),
        );
    let server = serve(app).await;
    let c = reqwest::Client::new();
    let response = c.get(format!("{}/body", server.url)).send().await.unwrap();
    assert!(
        net::limited(response, 8, Duration::from_secs(1))
            .await
            .is_err()
    );
    let response = c.get(format!("{}/body", server.url)).send().await.unwrap();
    assert_eq!(
        net::limited(response, 9, Duration::from_secs(1))
            .await
            .unwrap()
            .len(),
        9
    );
    let response = c.get(format!("{}/slow", server.url)).send().await.unwrap();
    assert!(
        net::limited(response, 9, Duration::from_millis(5))
            .await
            .is_err()
    );
}

#[test]
fn every_proxy_path_refusal_and_readonly_mode() {
    for (method, path) in [
        (Method::GET, "/hosts/connect"),
        (Method::GET, "/a/anything"),
        (Method::GET, "/install/script"),
        (Method::GET, "/api/../system/instance"),
        (Method::GET, "/api/%2e%2e/system/instance"),
        (Method::GET, "/api/%252e%252e/system/instance"),
        (Method::GET, "/api/projects%2fsecret"),
        (Method::GET, "/api/projects%5csecret"),
        (Method::GET, "/api//projects"),
        (Method::GET, "/api/projects\\secret"),
        (Method::POST, "/api/session"),
        (Method::DELETE, "/api/sessions"),
        (Method::PUT, "/api/auth/password"),
        (Method::POST, "/api/secrets/global/name/reveal"),
        (Method::POST, "/api/hosts"),
        (Method::DELETE, "/api/hosts/name"),
        (Method::POST, "/api/hosts/name/rotate"),
        (Method::POST, "/api/subscriptions/name/api-key"),
        (Method::GET, "/api/links"),
        (Method::POST, "/api/links/pair"),
        (Method::POST, "/api/backups"),
        (Method::GET, "/api/backups/name"),
        (Method::POST, "/api/system/update/rollback"),
        (Method::POST, "/api/config/push"),
        (Method::GET, "/api/remotes/lab/proxy/api/projects"),
        (Method::POST, "/api/schedules"),
        (Method::POST, "/api/projects/terra/rules"),
        (Method::PUT, "/api/system-prompts/leader"),
        (Method::PUT, "/api/skills/name"),
        (Method::POST, "/api/templates"),
        (Method::TRACE, "/api/projects"),
    ] {
        assert!(
            net::check_path(&method, path, false, false).is_err(),
            "{method} {path}"
        );
    }
    assert!(net::check_path(&Method::POST, "/api/projects", false, true).is_err());
    assert!(net::check_path(&Method::GET, "/api/agents/id/pty", true, true).is_err());
    assert!(net::check_path(&Method::GET, "/api/system/instance", true, false).is_err());
    assert!(net::check_path(&Method::GET, "/api/events", true, true).is_ok());
    assert!(net::check_path(&Method::GET, "/api/projects", false, true).is_ok());
}

#[test]
fn header_allowlists_drop_credentials_cookies_and_active_content() {
    let mut h = HeaderMap::new();
    for k in [
        "authorization",
        "cookie",
        "x-csrf-token",
        "origin",
        "x-forwarded-host",
        "x-terrarium-agent",
    ] {
        h.insert(k, "home-only".parse().unwrap());
    }
    h.insert("accept", "application/json".parse().unwrap());
    let out = net::request_headers(&h, "test-link-key", "home-id").unwrap();
    assert_eq!(out.len(), 3);
    assert!(out[header::AUTHORIZATION].is_sensitive());
    assert!(out[header::AUTHORIZATION] != "home-only");
    let mut h = HeaderMap::new();
    h.insert("content-type", "application/json".parse().unwrap());
    for k in [
        "set-cookie",
        "content-security-policy",
        "x-frame-options",
        "location",
    ] {
        h.insert(k, "untrusted".parse().unwrap());
    }
    h.insert(
        "content-disposition",
        "inline; filename=page.html".parse().unwrap(),
    );
    let out = net::response_headers(&h, "lab", false).unwrap();
    assert!(
        !out.contains_key("set-cookie")
            && !out.contains_key("content-disposition")
            && !out.contains_key("location")
    );
    assert_eq!(out["x-content-type-options"], "nosniff");
    for ct in [
        "text/html",
        "image/svg+xml",
        "text/javascript",
        "text/plain",
    ] {
        h.insert("content-type", ct.parse().unwrap());
        assert!(net::response_headers(&h, "lab", false).is_err());
        assert!(net::response_headers(&h, "lab", true).is_err());
    }
    assert_eq!(
        net::query(Some(
            "since=2&token=home-secret&%74oken=also-secret&access_token=secret"
        ))
        .unwrap(),
        "since=2"
    );
}

#[test]
fn origins_names_and_limits_are_fail_closed() {
    for u in [
        "http://example.com",
        "http://127.0.0.1@evil.test",
        "https://host/path",
        "https://host/?a=b",
        "https://host/#frag",
        "ftp://host",
    ] {
        assert!(net::check_url(u).is_err());
    }
    for n in ["", "Lab", "../lab", "lab/test", "a b"] {
        assert!(net::check_name(n).is_err());
    }
    for u in ["http://127.0.0.1", "https://lab.example"] {
        assert!(net::check_url(u).is_ok());
    }
    let runtime = net::Runtime::default();
    let l = runtime.get("remote");
    let permits: Vec<_> = (0..64).map(|_| l.acquire(false).unwrap()).collect();
    assert!(l.acquire(false).is_err());
    let streams: Vec<_> = (0..8).map(|_| l.acquire(true).unwrap()).collect();
    assert!(l.acquire(true).is_err());
    drop((permits, streams));
    assert!(l.acquire(true).is_ok());
    runtime.close("remote");
    assert!(runtime.get("remote").acquire(false).is_err());
    runtime.reset("remote");
    assert!(runtime.get("remote").acquire(false).is_ok());
}

struct Server {
    url: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn serve(app: Router) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Server { url, task }
}

async fn fake(home: &TestDaemon, app: Router, id: &str) -> Server {
    let expected = id.to_owned();
    let app = app.route(
        "/api/links/pair",
        axum::routing::post(move || {
            let expected = expected.clone();
            async move {
                Json(
                    json!({"key": format!("tlk_{}", "0".repeat(64)), "read_only":false,
            "instance":{"id":expected,"version":terrarium_version::VERSION}}),
                )
            }
        }),
    );
    let server = serve(app).await;
    home.post(
        "/api/remotes",
        json!({"name":"lab", "url":server.url,
        "instance_id":id,"code":format!("tlp_{}", "0".repeat(64))}),
    )
    .await
    .ok();
    server
}

#[tokio::test]
async fn hostile_http_headers_redirects_html_and_instance_mismatch() {
    let home = TestDaemon::start().await;
    let id = ulid::Ulid::new().to_string();
    let instance = id.clone();
    let app = Router::new()
        .route(
            "/api/system/instance",
            get(move || {
                let id = instance.clone();
                async move { Json(json!({"id":id,"version":terrarium_version::VERSION})) }
            }),
        )
        .route(
            "/api/html",
            get(|| async { ([("content-type", "text/html")], "<script>bad</script>") }),
        )
        .route(
            "/api/redirect",
            get(|| async { (StatusCode::FOUND, [("location", "/api/secret")]) }),
        )
        .route(
            "/api/json",
            get(|| async {
                (
                    [
                        ("content-type", "application/json"),
                        ("set-cookie", "evil=1"),
                        ("content-disposition", "inline"),
                    ],
                    "{}",
                )
            }),
        );
    let _server = fake(&home, app, &id).await;
    home.get("/api/remotes/lab/proxy/api/html")
        .await
        .err(502, "remote_unreachable");
    home.get("/api/remotes/lab/proxy/api/redirect")
        .await
        .err(502, "remote_unreachable");
    let json = home.get("/api/remotes/lab/proxy/api/json").await;
    json.ok();
    assert!(!json.headers.contains_key("set-cookie"));
    assert!(!json.headers.contains_key("content-disposition"));
    let r = home.daemon.as_ref().unwrap().state.clone();
    r.db.write("operator", |t| {
        t.execute("UPDATE remotes SET instance_id='wrong'", [])?;
        Ok(())
    })
    .await
    .unwrap();
    home.get("/api/remotes/lab/proxy/api/json")
        .await
        .err(502, "remote_mismatch");
}

#[tokio::test]
async fn event_websocket_and_sse_stream_and_removal() {
    let home = TestDaemon::start().await;
    let remote = remote().await;
    link(&home, &remote, false).await;
    let mut ws = home
        .ws("/api/remotes/lab/proxy/api/events?live=1", None)
        .await
        .unwrap();
    remote
        .post("/api/projects", json!({"name":"ws-event"}))
        .await
        .ok();
    remote
        .daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .write("operator", |t| {
            t.emit(terrariumd::events::EventDraft::new(
                "test",
                "remote test event",
                json!({}),
            ))?;
            Ok(())
        })
        .await
        .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(frame.is_text());
    // SSE stays streaming rather than waiting for the full response body.
    let state = home.daemon.as_ref().unwrap().state.clone();
    let (server, addr) = terrariumd::api::serve_tcp(state, "127.0.0.1:0", "home-test".into())
        .await
        .unwrap();
    let client = reqwest::Client::new();
    let mut response = client
        .get(format!(
            "http://{addr}/api/remotes/lab/proxy/api/events?since=0"
        ))
        .bearer_auth("home-test")
        .header("accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert!(
        tokio::time::timeout(Duration::from_secs(10), response.chunk())
            .await
            .unwrap()
            .unwrap()
            .is_some()
    );
    assert_eq!(home.delete("/api/remotes/lab").await.ok()["revoked"], true);
    let end = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .unwrap();
    assert!(end.is_none_or(|m| m.is_err() || m.is_ok_and(|m| m.is_close())));
    server.abort();
    let _ = ws.close(None).await;
}

#[tokio::test]
async fn tls_leaf_pin_is_checked_on_every_http_sse_and_ws_connection_before_key() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let certificate = terrarium_host::tls::self_signed(&["not-the-url.invalid".into()]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cert = dir.path().join("cert.pem");
    let key = dir.path().join("key.pem");
    std::fs::write(&cert, &certificate.cert_pem).unwrap();
    std::fs::write(&key, &certificate.key_pem).unwrap();
    let config = terrarium_host::tls::server_config(&cert, &key).unwrap().0;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let received = Arc::new(AtomicUsize::new(0));
    let seen = received.clone();
    let task = tokio::spawn(async move {
        let acceptor = tokio_rustls::TlsAcceptor::from(config);
        loop {
            let (tcp, _) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            let seen = seen.clone();
            tokio::spawn(async move {
                if let Ok(mut tls) = acceptor.accept(tcp).await {
                    let mut bytes = vec![0; 8192];
                    let n = tls.read(&mut bytes).await.unwrap_or(0);
                    if n > 0 {
                        seen.fetch_add(1, Ordering::SeqCst);
                    }
                    let _ = tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await;
                    let _ = tls.shutdown().await;
                }
            });
        }
    });
    let _server = Server {
        url: format!("https://{addr}"),
        task,
    };
    let mut r = terrariumd::store::remotes::Remote {
        id: "id".into(),
        name: "lab".into(),
        url: format!("https://{addr}/"),
        instance_id: "id".into(),
        pin_sha256: Some(certificate.fingerprint),
        ca_pem: None,
        label: "lab".into(),
        read_only: false,
        created_at: String::new(),
        last_ok_at: None,
        last_error: None,
        status: "offline".into(),
        calls_today: 0,
    };
    // The pin deliberately overrides the self-signed chain and wrong name.
    for (accept, upgrade) in [
        ("application/json", false),
        ("text/event-stream", false),
        ("application/json", true),
    ] {
        let c = net::client(&r, &[]).await.unwrap();
        let mut q = c
            .get(format!("{}api/events", r.url))
            .bearer_auth("test-link-key")
            .header("accept", accept);
        if upgrade {
            q = q
                .header("connection", "Upgrade")
                .header("upgrade", "websocket")
                .header("sec-websocket-version", "13")
                .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==");
        }
        q.send().await.unwrap();
    }
    assert_eq!(received.load(Ordering::SeqCst), 3);
    r.pin_sha256 = Some("00".repeat(32));
    for (accept, upgrade) in [
        ("application/json", false),
        ("text/event-stream", false),
        ("application/json", true),
    ] {
        let c = net::client(&r, &[]).await.unwrap();
        let mut q = c
            .get(format!("{}api/events", r.url))
            .bearer_auth("test-link-key")
            .header("accept", accept);
        if upgrade {
            q = q
                .header("connection", "Upgrade")
                .header("upgrade", "websocket")
                .header("sec-websocket-version", "13")
                .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==");
        }
        let error = q.send().await.expect_err("wrong leaf must fail TLS");
        assert!(
            net::connection_error(error)
                .details
                .is_some_and(|d| d["status"] == "pin_mismatch")
        );
    }
    assert_eq!(
        received.load(Ordering::SeqCst),
        3,
        "a mismatched leaf received HTTP bytes"
    );
}

#[tokio::test]
async fn sealed_key_rotation_and_missing_master_key_cover_remotes() {
    let home = TestDaemon::start().await;
    let remote = remote().await;
    let row = link(&home, &remote, false).await;
    let mut c = rusqlite::Connection::open(home.state.join("terrarium.db")).unwrap();
    assert!(c.table_exists(None, "remotes").unwrap());
    let id = row["id"].as_str().unwrap();
    let before = terrariumd::store::remotes::sealed(&c, id).unwrap();
    let key = terrariumd::secrets::key(&home.state).unwrap();
    assert!(
        key.open(&before, &terrariumd::store::remotes::aad(id))
            .is_some()
    );
    assert!(key.open(&before, b"remote:another-id").is_none());
    terrariumd::secrets::migrate::rotate_key(&mut c, &home.state).unwrap();
    let after = terrariumd::store::remotes::sealed(&c, id).unwrap();
    assert!(
        key.open(&after, &terrariumd::store::remotes::aad(id))
            .is_none()
    );
    home.get("/api/remotes/lab/proxy/api/projects").await.ok();
    std::fs::remove_file(home.state.join("secrets.key")).unwrap();
    assert!(terrariumd::secrets::ensure_key(&c, &home.state).is_err());
}
