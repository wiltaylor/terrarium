//! Webhook actions (docs/api.md §19.5): a trigger that sends an HTTP
//! request about each event. A local axum server is the receiver; no test
//! uses the outside network. The retries run with `webhooks::pass` at a
//! time ahead of the real clock, so the daemon's own worker does not send
//! them first.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use common::TestDaemon;
use serde_json::{Value, json};
use terrariumd::api::Shared;
use terrariumd::events::EventDraft;
use terrariumd::webhooks::{self, net::PrivateMode};

fn state(td: &TestDaemon) -> Shared {
    td.daemon.as_ref().unwrap().state.clone()
}

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

const TOKEN: &str = "whk_9fQ2xLr7Tz4mB8vN1cY6aP3s";
const SIGN: &str = "sig_K8dW3nQ7rT1yV5bX9mZ2cF6h";

/// One request that the receiver got.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: String,
}

/// One answer of the receiver: the status, the headers, and the body.
type Reply = (u16, Vec<(&'static str, String)>, String);

/// The receiver: it keeps each request and answers from a queue (200 "ok"
/// when the queue is empty).
#[derive(Clone, Default)]
struct Hook {
    seen: Arc<Mutex<Vec<Seen>>>,
    replies: Arc<Mutex<VecDeque<Reply>>>,
}

impl Hook {
    fn reply(&self, status: u16, headers: Vec<(&'static str, String)>, body: &str) {
        self.replies
            .lock()
            .unwrap()
            .push_back((status, headers, body.to_owned()));
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

async fn receive(
    State(h): State<Hook>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    h.seen.lock().unwrap().push(Seen {
        method: method.to_string(),
        path: uri.path().to_owned(),
        headers: headers
            .iter()
            .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
            .collect(),
        body: String::from_utf8_lossy(&body).into_owned(),
    });
    let next = h.replies.lock().unwrap().pop_front();
    let (status, extra, text) = next.unwrap_or((200, Vec::new(), "ok".into()));
    let mut r = (StatusCode::from_u16(status).unwrap(), text).into_response();
    for (k, v) in extra {
        r.headers_mut().insert(k, v.parse().unwrap());
    }
    r
}

/// Serve a receiver on 127.0.0.1. Returns it and its base URL.
async fn serve() -> (Hook, String, u16) {
    let hook = Hook::default();
    let app = Router::new().fallback(receive).with_state(hook.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (hook, format!("http://127.0.0.1:{port}"), port)
}

/// Write one event into the log of project `terra`, as an event source
/// does.
async fn emit(td: &TestDaemon, kind: &str, data: Value) -> i64 {
    let kind = kind.to_owned();
    state(td)
        .db
        .write("source:terra/gh", move |t| {
            let summary = format!("{kind}: {}", data["to"].as_str().unwrap_or("?"));
            Ok(
                t.emit(EventDraft::new(kind, summary, data).project("terra"))?
                    .seq,
            )
        })
        .await
        .unwrap()
}

/// The deliveries of a schedule, newest first.
async fn deliveries(td: &TestDaemon, sid: &str) -> Vec<Value> {
    td.get(&format!("/api/schedules/{sid}/deliveries?limit=200"))
        .await
        .ok()
        .as_array()
        .unwrap()
        .clone()
}

/// Wait until the deliveries of `sid` satisfy `f`.
async fn wait_for(td: &TestDaemon, sid: &str, f: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    for _ in 0..500 {
        let d = deliveries(td, sid).await;
        if f(&d) {
            return d;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "the deliveries of {sid} did not get there: {:#?}",
        deliveries(td, sid).await
    );
}

/// Run a worker pass at `secs` seconds from now and wait for its sends.
async fn pass_at(td: &TestDaemon, secs: i64) -> Vec<Value> {
    let now = Utc::now() + chrono::Duration::seconds(secs);
    let mut out = Vec::new();
    for h in webhooks::pass(&state(td), now).await {
        if let Some(d) = h.await.unwrap() {
            out.push(serde_json::to_value(d).unwrap());
        }
    }
    out
}

async fn setup(td: &TestDaemon) -> String {
    td.project("terra").await;
    let lead = td
        .agent("terra", "lead", json!({"roles": ["project_admin"]}))
        .await;
    td.agent("terra", "worker", json!({})).await;
    td.put(
        "/api/secrets/global/hook-token",
        json!({"kind": "api_key", "fields": {"value": TOKEN}}),
    )
    .await
    .ok();
    td.put(
        "/api/secrets/global/hook-sign",
        json!({"kind": "api_key", "fields": {"value": SIGN}}),
    )
    .await
    .ok();
    id(&lead)
}

fn trigger(name: &str, action: Value) -> Value {
    json!({"name": name, "project": "terra",
           "spec": {"kind": "on", "kinds": ["monitor.status"]}, "action": action})
}

async fn create(td: &TestDaemon, name: &str, action: Value) -> String {
    let r = td.post("/api/schedules", trigger(name, action)).await;
    assert_eq!(r.status, 201, "{}", r.body);
    id(&r.body)
}

fn status(v: &Value) -> &str {
    v["status"].as_str().unwrap_or_default()
}

#[tokio::test]
async fn a_monitor_event_reaches_the_receiver_signed_with_secret_headers() {
    let td = TestDaemon::start().await;
    setup(&td).await;
    let (hook, base, _) = serve().await;
    let sid = create(
        &td,
        "page",
        json!({"type": "webhook", "url": format!("{base}/in?k=v"),
               "headers": [{"name": "Authorization", "secret": "hook-token"},
                           {"name": "X-Env", "value": "prod"}],
               "signing": {"secret": "hook-sign"}}),
    )
    .await;
    // The receiver echoes the token: the log must not show it.
    hook.reply(200, vec![], &format!("thanks {TOKEN}"));
    let seq = emit(
        &td,
        "monitor.status",
        json!({"monitor": "disk", "from": "ok", "to": "crit", "value": 93}),
    )
    .await;
    let d = wait_for(&td, &sid, |d| d.iter().any(|x| status(x) == "ok")).await;
    let seen = hook.seen();
    assert_eq!(seen.len(), 1);
    let s = &seen[0];
    assert_eq!(s.method, "POST");
    assert_eq!(s.path, "/in");
    assert_eq!(s.headers["content-type"], "application/json");
    assert_eq!(s.headers["authorization"], TOKEN);
    assert_eq!(s.headers["x-env"], "prod");
    assert_eq!(s.headers["x-terrarium-event"], "monitor.status");
    assert_eq!(
        s.headers["x-terrarium-delivery"],
        d[0]["id"].as_str().unwrap()
    );
    assert!(s.headers.contains_key("x-terrarium-timestamp"));
    assert!(s.headers["user-agent"].starts_with("terrarium/"));
    // The signature is the HMAC-SHA256 of the raw body.
    assert_eq!(
        s.headers["x-terrarium-signature"],
        format!(
            "sha256={}",
            terrariumd::ingest::sign(SIGN, s.body.as_bytes())
        )
    );
    let body: Value = serde_json::from_str(&s.body).unwrap();
    assert_eq!(body["event"]["kind"], "monitor.status");
    assert_eq!(body["event"]["seq"], seq);
    assert_eq!(body["event"]["project"], "terra");
    assert_eq!(body["event"]["data"]["to"], "crit");
    assert_eq!(body["trigger"]["name"], "page");
    assert_eq!(body["trigger"]["project"], "terra");
    // The log: the URL has no query, the response no secret.
    let row = &d[0];
    assert_eq!(row["url"], format!("{base}/in?…"));
    assert_eq!(row["http_status"], 200);
    assert_eq!(row["attempt"], 1);
    assert_eq!(row["event_seq"], seq);
    let resp = row["response"].as_str().unwrap();
    assert!(
        resp.starts_with("thanks ") && !resp.contains(TOKEN),
        "{resp}"
    );
    assert!(!row.to_string().contains(TOKEN));
    assert!(!row.to_string().contains(SIGN));
    // The run is ok, and a `webhook.delivery` event names the trigger.
    let runs = td.get(&format!("/api/schedules/{sid}/runs")).await;
    assert!(runs.ok()[0]["summary"].as_str().unwrap().contains("queued"));
    let ev = td.get("/api/events?kind=webhook.delivery&limit=50").await;
    let ev = ev.ok().as_array().unwrap().clone();
    let e = ev
        .iter()
        .find(|e| e["data"]["trigger"] == sid.as_str())
        .unwrap();
    assert_eq!(e["data"]["status"], "ok");
    assert_eq!(e["data"]["http_status"], 200);
    assert_eq!(e["project"], "terra");
    assert_eq!(e["actor"], format!("schedule:{sid}"));
}

#[tokio::test]
async fn formats_templates_and_the_test_send() {
    let td = TestDaemon::start().await;
    setup(&td).await;
    let (hook, base, _) = serve().await;
    // A template that does not render JSON for a JSON content type: 422.
    let r = td
        .post(
            "/api/schedules",
            trigger(
                "bad",
                json!({"type": "webhook", "url": base, "format": "template",
                       "template": "{\"s\": {{event.summary}}}"}),
            ),
        )
        .await;
    r.err(422, "invalid");
    assert_eq!(r.body["error"]["details"]["field"], "action.template");
    let r = td
        .post(
            "/api/schedules",
            trigger(
                "bad",
                json!({"type": "webhook", "url": base, "format": "template",
                       "template": "{{event.nope.x}} {{oops}}"}),
            ),
        )
        .await;
    assert_eq!(r.body["error"]["details"]["field"], "action.template");
    let sid = create(
        &td,
        "tpl",
        json!({"type": "webhook", "url": base, "method": "PUT", "format": "template",
               "template": "{\"m\": \"{{event.data.monitor}}\", \"s\": \"{{event.summary}}\", \"v\": {{event.data.value | json}}, \"x\": \"{{event.data.missing}}\", \"t\": \"{{trigger.name}}\"}",
               "headers": [{"name": "X-Key", "secret": "hook-token"}]}),
    )
    .await;
    // The test send: a sample event of the trigger's kind, sent now.
    let r = td.post_empty(&format!("/api/schedules/{sid}/test")).await;
    let t = r.ok().clone();
    assert_eq!(t["delivery"]["status"], "ok", "{t}");
    assert_eq!(t["delivery"]["test"], true);
    assert_eq!(t["request"]["method"], "PUT");
    let headers = t["request"]["headers"].as_array().unwrap();
    assert!(
        headers.contains(&json!(["X-Key", "[secret:hook-token]"])),
        "{headers:?}"
    );
    assert!(!t.to_string().contains(TOKEN));
    let seen = hook.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "PUT");
    assert_eq!(seen[0].headers["x-key"], TOKEN);
    let body: Value = serde_json::from_str(&seen[0].body).unwrap();
    assert_eq!(body["m"], "disk-usage");
    assert_eq!(body["v"], 93);
    assert_eq!(body["x"], "");
    assert_eq!(body["t"], "tpl");
    assert!(body["s"].as_str().unwrap().contains("ok → crit"));
    // Slack, Discord, and ntfy presets.
    for (name, format, check) in [
        ("slack", "slack", "text"),
        ("discord", "discord", "content"),
        ("ntfy", "ntfy", ""),
    ] {
        let sid = create(
            &td,
            name,
            json!({"type": "webhook", "url": base, "format": format}),
        )
        .await;
        let t = td.post_empty(&format!("/api/schedules/{sid}/test")).await;
        assert_eq!(t.ok()["delivery"]["status"], "ok", "{}", t.body);
        let last = hook.seen().pop().unwrap();
        if check.is_empty() {
            assert_eq!(last.headers["priority"], "5");
            assert_eq!(last.headers["title"], "terra: monitor.status");
            assert!(last.headers["content-type"].starts_with("text/plain"));
            assert!(last.body.contains("ok → crit"));
        } else {
            let v: Value = serde_json::from_str(&last.body).unwrap();
            assert!(
                v[check]
                    .as_str()
                    .unwrap()
                    .starts_with("[terra] monitor.status: "),
                "{v}"
            );
        }
    }
    // A test of a schedule that is not a webhook runs its action (#15).
    let m = td
        .post(
            "/api/schedules",
            trigger(
                "mail",
                json!({"type": "mail", "to": "operator", "body": "x"}),
            ),
        )
        .await;
    let t = td
        .post_empty(&format!("/api/schedules/{}/test", id(&m.body)))
        .await;
    assert_eq!(t.ok()["action"], "mail", "{}", t.body);
    assert_eq!(t.body["status"], "ok", "{}", t.body);
    assert!(t.body.get("delivery").is_none(), "{}", t.body);
}

#[tokio::test]
async fn secret_headers_need_a_use_right_at_create_and_at_send() {
    let td = TestDaemon::start().await;
    let lead = setup(&td).await;
    let (hook, base, _) = serve().await;
    let action = json!({"type": "webhook", "url": base,
                        "headers": [{"name": "Authorization", "secret": "hook-token"}]});
    // A global secret with no grant: 403 for the project_admin.
    let r = td
        .post_as(&lead, "/api/schedules", trigger("page", action.clone()))
        .await;
    r.err(403, "forbidden");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("grant")
    );
    let mut sign = action.clone();
    sign["headers"] = json!([]);
    sign["signing"] = json!({"secret": "hook-sign"});
    td.post_as(&lead, "/api/schedules", trigger("sign", sign))
        .await
        .err(403, "forbidden");
    // A member cannot make a trigger at all; a missing secret is 422.
    td.post_as(
        "terra/worker",
        "/api/schedules",
        trigger("page", action.clone()),
    )
    .await
    .err(403, "forbidden");
    let mut missing = action.clone();
    missing["headers"][0]["secret"] = json!("nope");
    let r = td.post("/api/schedules", trigger("page", missing)).await;
    r.err(422, "invalid");
    assert_eq!(r.body["error"]["details"]["field"], "action.headers");
    // A project secret: the project_admin manages it, so it may use it.
    td.put_as(
        &lead,
        "/api/secrets/terra/proj-key",
        json!({"kind": "api_key", "fields": {"value": "pk_7Hc2Lq9Vx4Ns1Bf8Mz3R"}}),
    )
    .await
    .ok();
    let mut own = action.clone();
    own["headers"][0]["secret"] = json!("proj-key");
    let r = td
        .post_as(&lead, "/api/schedules", trigger("own", own))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    // Only one trigger below takes the events.
    td.delete(&format!("/api/schedules/{}", id(&r.body))).await;
    // With a `use` grant to the project, the global secret works.
    td.post(
        "/api/secrets/global/hook-token/grants",
        json!({"grantee": "project:terra", "level": "use"}),
    )
    .await
    .ok();
    let r = td
        .post_as(&lead, "/api/schedules", trigger("page", action.clone()))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let sid = id(&r.body);
    assert_eq!(r.body["run_as"], format!("agent:{lead}"));
    let t = td
        .post_empty_as(&lead, &format!("/api/schedules/{sid}/test"))
        .await;
    assert_eq!(t.ok()["delivery"]["status"], "ok", "{}", t.body);
    assert_eq!(hook.seen().len(), 1);
    // Revoke the grant: the test send is 403, and a queued delivery is
    // refused at its attempt, with no request.
    td.delete("/api/secrets/global/hook-token/grants?grantee=project:terra")
        .await;
    td.post_empty(&format!("/api/schedules/{sid}/test"))
        .await
        .err(403, "forbidden");
    emit(&td, "monitor.status", json!({"to": "crit"})).await;
    let d = wait_for(&td, &sid, |d| d.iter().any(|x| status(x) == "failed")).await;
    let failed = d.iter().find(|x| status(x) == "failed").unwrap();
    assert!(
        failed["error"].as_str().unwrap().starts_with("refused:"),
        "{failed}"
    );
    assert_eq!(hook.seen().len(), 1);
}

#[tokio::test]
async fn retries_back_off_respect_retry_after_and_stop_on_a_4xx() {
    let td = TestDaemon::start().await;
    setup(&td).await;
    let (hook, base, _) = serve().await;
    let sid = create(&td, "page", json!({"type": "webhook", "url": base})).await;
    // A 503 with Retry-After: 120 s, then a 200.
    hook.reply(503, vec![("retry-after", "120".into())], "busy");
    emit(&td, "monitor.status", json!({"to": "crit"})).await;
    let d = wait_for(&td, &sid, |d| d.iter().any(|x| x["attempt"] == 1)).await;
    let row = &d[0];
    assert_eq!(status(row), "pending");
    assert_eq!(row["http_status"], 503);
    let next = chrono::DateTime::parse_from_rfc3339(row["next_attempt_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&Utc);
    let wait = (next - Utc::now()).num_seconds();
    assert!((100..=121).contains(&wait), "{wait}");
    // Not due at +60 s; due at +125 s.
    assert!(pass_at(&td, 60).await.is_empty());
    let sent = pass_at(&td, 125).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(status(&sent[0]), "ok");
    assert_eq!(sent[0]["attempt"], 2);
    assert_eq!(hook.seen().len(), 2);

    // A 404: no retry.
    hook.reply(404, vec![], "no such hook");
    emit(&td, "monitor.status", json!({"to": "warn"})).await;
    let d = wait_for(&td, &sid, |d| d.iter().any(|x| status(x) == "failed")).await;
    let failed = d.iter().find(|x| status(x) == "failed").unwrap();
    assert_eq!(failed["http_status"], 404);
    assert!(failed["next_attempt_at"].is_null());
    assert_eq!(failed["response"], "no such hook");

    // 5xx every time: 6 attempts on the backoff, then `giving_up`.
    for _ in 0..6 {
        hook.reply(500, vec![], "down");
    }
    emit(&td, "monitor.status", json!({"to": "crit"})).await;
    let d = wait_for(&td, &sid, |d| {
        d.iter()
            .any(|x| status(x) == "pending" && x["http_status"] == 500)
    })
    .await;
    let did = d[0]["id"].as_str().unwrap().to_owned();
    for (n, secs) in [(2, 11), (3, 72), (4, 380), (5, 2200), (6, 9500)] {
        let sent = pass_at(&td, secs).await;
        let row = sent.iter().find(|x| x["id"] == did.as_str()).unwrap();
        assert_eq!(row["attempt"], n);
        if n < 6 {
            assert_eq!(status(row), "pending");
        } else {
            assert_eq!(status(row), "giving_up");
        }
    }
}

#[tokio::test]
async fn a_pending_delivery_survives_a_restart() {
    let mut td = TestDaemon::start().await;
    setup(&td).await;
    let (hook, base, _) = serve().await;
    let sid = create(&td, "page", json!({"type": "webhook", "url": base})).await;
    hook.reply(502, vec![], "bad gateway");
    emit(&td, "monitor.status", json!({"to": "crit"})).await;
    wait_for(&td, &sid, |d| d.iter().any(|x| x["attempt"] == 1)).await;
    td.boot(None, None).await;
    let d = deliveries(&td, &sid).await;
    assert_eq!(status(&d[0]), "pending");
    let sent = pass_at(&td, 15).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(status(&sent[0]), "ok");
    assert_eq!(hook.seen().len(), 2);
}

/// The disable notices in the operator's mail.
async fn operator_notices(td: &TestDaemon) -> usize {
    td.get("/api/mail?to=operator")
        .await
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["subject"] == "Webhook trigger terra/page disabled")
        .count()
}

#[tokio::test]
async fn twenty_failures_in_a_row_disable_the_trigger_and_mail_once() {
    let td = TestDaemon::start().await;
    setup(&td).await;
    let (hook, base, _) = serve().await;
    let sid = create(&td, "page", json!({"type": "webhook", "url": base})).await;
    let key = sid.clone();
    state(&td)
        .db
        .write("system", move |t| {
            t.execute(
                "UPDATE schedules SET webhook_failures = 19 WHERE id = ?1",
                [&key],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    hook.reply(410, vec![], "gone");
    emit(&td, "monitor.status", json!({"to": "crit"})).await;
    wait_for(&td, &sid, |d| d.iter().any(|x| status(x) == "failed")).await;
    let s = td.get(&format!("/api/schedules/{sid}")).await.ok().clone();
    assert_eq!(s["enabled"], false);
    assert!(
        s["disabled_reason"]
            .as_str()
            .unwrap()
            .contains("20 webhook attempts in a row failed"),
        "{s}"
    );
    assert_eq!(operator_notices(&td).await, 1);
    // A redeliver of a disabled trigger waits: no send, no second mail.
    let failed = deliveries(&td, &sid).await[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let r = td
        .post_empty(&format!(
            "/api/schedules/{sid}/deliveries/{failed}/redeliver"
        ))
        .await;
    assert_eq!(r.status, 202, "{}", r.body);
    assert_eq!(r.body["redelivery_of"], failed.as_str());
    assert!(pass_at(&td, 30).await.is_empty());
    assert_eq!(hook.seen().len(), 1);
    assert_eq!(operator_notices(&td).await, 1);
    // The enable sets the count to zero; the redelivery goes out.
    let s = td
        .post_empty(&format!("/api/schedules/{sid}/enable"))
        .await
        .ok()
        .clone();
    assert!(s.get("webhook_failures").is_none(), "{s}");
    let sent = pass_at(&td, 30).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(status(&sent[0]), "ok");
    assert_eq!(operator_notices(&td).await, 1);
}

#[tokio::test]
async fn the_rate_limit_drops_and_counts_extra_events() {
    let td = TestDaemon::start().await;
    setup(&td).await;
    let (hook, base, _) = serve().await;
    state(&td).webhooks.set_rate(2);
    let sid = create(&td, "page", json!({"type": "webhook", "url": base})).await;
    for to in ["a", "b", "c", "d"] {
        emit(&td, "monitor.status", json!({"to": to})).await;
    }
    let d = wait_for(&td, &sid, |d| {
        d.iter().filter(|x| status(x) == "ok").count() == 2
            && d.iter()
                .any(|x| status(x) == "dropped" && x["dropped"] == 2)
    })
    .await;
    let note = d.iter().find(|x| status(x) == "dropped").unwrap();
    assert!(note["error"].as_str().unwrap().contains("2 events dropped"));
    assert_eq!(hook.seen().len(), 2);
    let runs = td.get(&format!("/api/schedules/{sid}/runs")).await;
    let skipped = runs
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["summary"].as_str().unwrap().contains("rate limit"))
        .count();
    assert_eq!(skipped, 2);
    // A dropped row has nothing to redeliver.
    td.post_empty(&format!(
        "/api/schedules/{sid}/deliveries/{}/redeliver",
        note["id"].as_str().unwrap()
    ))
    .await
    .err(422, "invalid");
}

#[tokio::test]
async fn ssrf_rules_schemes_credentials_redirects_and_private_addresses() {
    let td = TestDaemon::start().await;
    setup(&td).await;
    let (hook, base, port) = serve().await;
    let (other, other_base, _) = serve().await;
    // Bad schemes and credentials: 422 at create.
    for url in [
        "ftp://example.test/x",
        "file:///etc/passwd",
        "https://user:pw@example.test/x",
        "nope",
    ] {
        let r = td
            .post(
                "/api/schedules",
                trigger("x", json!({"type": "webhook", "url": url})),
            )
            .await;
        r.err(422, "invalid");
        assert_eq!(r.body["error"]["details"]["field"], "action.url", "{url}");
    }
    // A redirect is not followed.
    let sid = create(&td, "redir", json!({"type": "webhook", "url": base})).await;
    hook.reply(302, vec![("location", format!("{other_base}/x"))], "");
    let t = td.post_empty(&format!("/api/schedules/{sid}/test")).await;
    let d = &t.ok()["delivery"];
    assert_eq!(d["status"], "failed");
    assert_eq!(d["http_status"], 302);
    assert!(d["error"].as_str().unwrap().contains("redirect"), "{d}");
    assert!(other.seen().is_empty());

    // Allow mode: a name resolves once, and the request goes to that
    // address (here a fake name for the local receiver).
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let rt = state(&td).webhooks.clone();
    rt.set_resolver(Some(Arc::new(move |h: &str, _p: u16| {
        c.fetch_add(1, Ordering::SeqCst);
        match h {
            "hook.test" => vec!["127.0.0.1".parse::<IpAddr>().unwrap()],
            "lan.test" => vec!["10.20.30.40".parse().unwrap()],
            // Public first; a second lookup would give loopback.
            "rebind.test" => {
                if calls_rebind() == 0 {
                    vec!["203.0.113.7".parse().unwrap()]
                } else {
                    vec!["127.0.0.1".parse().unwrap()]
                }
            }
            _ => Vec::new(),
        }
    })));
    let named = create(
        &td,
        "named",
        json!({"type": "webhook", "url": format!("http://hook.test:{port}/named")}),
    )
    .await;
    let before = hook.seen().len();
    let t = td.post_empty(&format!("/api/schedules/{named}/test")).await;
    assert_eq!(t.ok()["delivery"]["status"], "ok", "{}", t.body);
    let last = hook.seen().pop().unwrap();
    assert_eq!(last.path, "/named");
    assert_eq!(last.headers["host"], format!("hook.test:{port}"));
    assert_eq!(hook.seen().len(), before + 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // Deny mode: loopback (IPv4 and IPv6), private ranges, and a name
    // that resolves to one are refused before any connect.
    rt.set_private_mode(PrivateMode::Deny);
    let count = hook.seen().len();
    for (name, url, want) in [
        ("lo4", base.clone(), "loopback"),
        ("lo6", format!("http://[::1]:{port}/"), "loopback"),
        (
            "named-deny",
            format!("http://hook.test:{port}/"),
            "loopback",
        ),
        ("lan", "http://lan.test/hook".to_owned(), "private"),
        (
            "meta",
            "http://169.254.169.254/latest".to_owned(),
            "link-local",
        ),
        ("ula", "http://[fd00::1]/".to_owned(), "unique-local"),
        (
            "mapped",
            format!("http://[::ffff:127.0.0.1]:{port}/"),
            "loopback",
        ),
    ] {
        let sid = create(&td, name, json!({"type": "webhook", "url": url})).await;
        let t = td.post_empty(&format!("/api/schedules/{sid}/test")).await;
        let d = &t.ok()["delivery"];
        assert_eq!(d["status"], "failed", "{name}: {d}");
        let e = d["error"].as_str().unwrap();
        assert!(e.starts_with("refused:") && e.contains(want), "{name}: {e}");
    }
    assert_eq!(hook.seen().len(), count);
    // DNS rebinding: one lookup per attempt, and the connect goes to the
    // address that the check saw (a public one here), never to a second
    // answer.
    let rebind_calls = calls.load(Ordering::SeqCst);
    let sid = create(
        &td,
        "rebind",
        json!({"type": "webhook", "url": format!("http://rebind.test:{port}/"),
               "timeout_secs": 1}),
    )
    .await;
    let t = td.post_empty(&format!("/api/schedules/{sid}/test")).await;
    let d = &t.ok()["delivery"];
    assert_eq!(d["status"], "failed", "{d}");
    assert!(!d["error"].as_str().unwrap().starts_with("refused:"), "{d}");
    assert_eq!(calls.load(Ordering::SeqCst), rebind_calls + 1);
    assert_eq!(hook.seen().len(), count);
    // Back to allow: the local receiver works again.
    rt.set_private_mode(PrivateMode::Allow);
    let t = td.post_empty(&format!("/api/schedules/{named}/test")).await;
    assert_eq!(t.ok()["delivery"]["status"], "ok");
}

/// The rebinding resolver answers public the first time only. One static
/// counter is enough: one test uses it.
fn calls_rebind() -> usize {
    static N: AtomicUsize = AtomicUsize::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}

#[tokio::test]
async fn a_time_schedule_can_send_a_webhook_and_the_token_never_leaves() {
    let td = TestDaemon::start().await;
    setup(&td).await;
    let (hook, base, _) = serve().await;
    let r = td
        .post(
            "/api/schedules",
            json!({"name": "hourly", "project": "terra",
                   "spec": {"kind": "interval", "every": "1h"},
                   "action": {"type": "webhook", "url": base, "format": "slack"}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let sid = id(&r.body);
    let run = td.post_empty(&format!("/api/schedules/{sid}/run")).await;
    assert_eq!(run.ok()["status"], "ok", "{}", run.body);
    wait_for(&td, &sid, |d| d.iter().any(|x| status(x) == "ok")).await;
    let v: Value = serde_json::from_str(&hook.seen()[0].body).unwrap();
    assert!(v["text"].as_str().unwrap().contains("schedule.tick"), "{v}");
    // The daemon's API token is never sent, also from a header value.
    state(&td)
        .webhooks
        .set_own_token(Some("tok_daemon_api_9X2k"));
    let sid = create(
        &td,
        "leak",
        json!({"type": "webhook", "url": base,
               "headers": [{"name": "X-Leak", "value": "Bearer tok_daemon_api_9X2k"}]}),
    )
    .await;
    let t = td.post_empty(&format!("/api/schedules/{sid}/test")).await;
    let d = &t.ok()["delivery"];
    assert_eq!(d["status"], "failed");
    assert!(d["error"].as_str().unwrap().contains("API token"), "{d}");
    assert_eq!(hook.seen().len(), 1);
}
