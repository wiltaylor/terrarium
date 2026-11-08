//! Redaction (PLAN §16b.6, docs/api.md §5.2): the daemon replaces each
//! known secret value with `[secret:<label>]` before it stores or shows
//! mail, cards, wiki pages, memories, questions, events, audit details,
//! and transcripts.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::path::PathBuf;
use std::time::Duration;

use common::TestDaemon;
use serde_json::{Value, json};

const GLOBAL_VALUE: &str = "ghp_REDACTme0123456789";
const PROJECT_VALUE: &str = "sk-PROJECT-redact-me";
const SUB_KEY: &str = "sk-SUBSCRIPTION-key-0042";
/// Seven bytes: shorter than the matcher's minimum, so never redacted.
const SHORT_VALUE: &str = "pin1234";

/// The text of the database files (with the WAL), lossy.
fn db_text(td: &TestDaemon) -> String {
    let mut out = String::new();
    for name in ["terrarium.db", "terrarium.db-wal"] {
        let p: PathBuf = td.state.join(name);
        if let Ok(b) = std::fs::read(p) {
            out.push_str(&String::from_utf8_lossy(&b));
        }
    }
    out
}

fn assert_redacted(what: &str, text: &str) {
    for v in [GLOBAL_VALUE, PROJECT_VALUE, SUB_KEY] {
        assert!(!text.contains(v), "{what} holds `{v}`: {text}");
    }
}

async fn store_secrets(td: &TestDaemon) {
    td.post(
        "/api/secrets",
        json!({"name": "gh", "kind": "token", "fields": {"value": GLOBAL_VALUE}}),
    )
    .await
    .ok();
    td.post(
        "/api/secrets",
        json!({"scope": "p", "name": "deploy", "kind": "api_key",
               "fields": {"value": PROJECT_VALUE}}),
    )
    .await
    .ok();
    td.post(
        "/api/secrets",
        json!({"name": "door", "kind": "generic", "fields": {"pin": SHORT_VALUE},
               "secret_fields": ["pin"]}),
    )
    .await
    .ok();
    td.post(
        "/api/subscriptions/oa/api-key",
        json!({"provider": "openai", "key": SUB_KEY}),
    )
    .await
    .ok();
}

/// The text that each write carries: every secret, and the short one.
fn leaky(prefix: &str) -> String {
    format!("{prefix}: gh={GLOBAL_VALUE} deploy={PROJECT_VALUE} sub={SUB_KEY} pin={SHORT_VALUE}")
}

const EXPECTED: &str =
    "gh=[secret:gh] deploy=[secret:p/deploy] sub=[secret:subscription/oa] pin=pin1234";

#[tokio::test]
async fn stored_records_hold_the_label_not_the_value() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let w = td
        .agent("p", "w", json!({"roles": ["project_admin"]}))
        .await;
    let wid = w["id"].as_str().unwrap().to_owned();
    td.post_empty(&format!("/api/agents/{wid}/start"))
        .await
        .ok();
    td.wait_status(&wid, "idle").await;
    store_secrets(&td).await;

    // Mail.
    let m = td
        .post(
            "/api/mail",
            json!({"to": "p/w", "subject": leaky("subject"), "body": leaky("body")}),
        )
        .await;
    let m = m.ok().clone();
    assert!(m["body"].as_str().unwrap().contains(EXPECTED), "{m}");
    assert!(m["subject"].as_str().unwrap().contains(EXPECTED), "{m}");
    let shown = td
        .get(&format!("/api/mail/{}", m["id"].as_str().unwrap()))
        .await;
    assert_redacted("the mail", &shown.ok().to_string());

    // A card, and a comment on it.
    let c = td
        .post(
            "/api/projects/p/cards",
            json!({"title": leaky("title"), "body": leaky("card"), "comment": leaky("note")}),
        )
        .await;
    let c = c.ok().clone();
    assert!(c["body"].as_str().unwrap().contains(EXPECTED), "{c}");
    assert_redacted("the card", &c.to_string());

    // A wiki page.
    let p = td
        .post(
            "/api/projects/p/wiki",
            json!({"path": "notes", "body": format!("# Notes\n\n{}\n", leaky("page")),
                   "summary": leaky("summary")}),
        )
        .await;
    let p = p.ok().clone();
    assert!(p["body"].as_str().unwrap().contains(EXPECTED), "{p}");
    assert_redacted("the wiki page", &p.to_string());

    // A memory node.
    let n = td
        .post(
            "/api/projects/p/memories",
            json!({"title": "creds", "body": leaky("memory")}),
        )
        .await;
    let n = n.ok().clone();
    assert_redacted("the memory", &n.to_string());
    assert!(n.to_string().contains(EXPECTED), "{n}");

    // A question and its answer; the answer goes to the audit detail.
    let q = td
        .post_as(
            &wid,
            &format!("/api/agents/{wid}/questions"),
            json!({"text": leaky("question")}),
        )
        .await;
    assert_eq!(q.status, 201, "{}", q.body);
    // The writer is an agent of `p`: the names of `p` show, the others are
    // `[secret]` (PLAN §16b.6, S5).
    assert!(
        q.body["text"]
            .as_str()
            .unwrap()
            .contains("gh=[secret] deploy=[secret:p/deploy] sub=[secret] pin=pin1234"),
        "{}",
        q.body
    );
    let qid = q.body["id"].as_str().unwrap();
    td.post(
        &format!("/api/questions/{qid}/answer"),
        json!({"answer": leaky("answer")}),
    )
    .await
    .ok();
    let audit = td.get("/api/audit?action=question.answer").await;
    let audit = audit.ok().to_string();
    assert!(audit.contains("[secret:gh]"), "{audit}");
    assert_redacted("the audit", &audit);

    // An event from outside (a trigger payload).
    let e = td
        .post(
            "/api/events",
            json!({"kind": "custom.leak", "project": "p", "summary": leaky("summary"),
                   "data": {"token": GLOBAL_VALUE}}),
        )
        .await;
    assert_eq!(e.status, 201, "{}", e.body);
    assert_eq!(e.body["data"]["token"], "[secret:gh]");
    assert_redacted("the event", &e.body.to_string());

    // The database files hold no value.
    assert_redacted("the database", &db_text(&td));

    // A delete of a secret: new writes keep its value.
    td.delete("/api/secrets/global/gh").await;
    let m = td
        .post("/api/mail", json!({"to": "p/w", "body": leaky("later")}))
        .await;
    let body = m.ok()["body"].as_str().unwrap().to_owned();
    assert!(body.contains(&format!("gh={GLOBAL_VALUE}")), "{body}");
    assert!(body.contains("deploy=[secret:p/deploy]"), "{body}");
}

#[tokio::test]
async fn the_matcher_is_built_again_at_the_start() {
    let mut td = TestDaemon::start().await;
    td.project("p").await;
    store_secrets(&td).await;
    td.boot(None, None).await;
    td.agent("p", "w", json!({})).await;
    let m = td
        .post(
            "/api/mail",
            json!({"to": "p/w", "body": leaky("after a restart")}),
        )
        .await;
    assert!(
        m.ok()["body"].as_str().unwrap().contains(EXPECTED),
        "{}",
        m.body
    );
}

#[tokio::test]
async fn transcripts_are_redacted_at_read_time() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let a = td.agent("p", "w", json!({})).await;
    let id = a["id"].as_str().unwrap().to_owned();
    td.post_empty(&format!("/api/agents/{id}/start")).await.ok();
    td.wait_status(&id, "idle").await;
    let mut s = Value::Null;
    for _ in 0..200 {
        let r = td.get(&format!("/api/agents/{id}/sessions")).await;
        s = r.ok()[0].clone();
        if s["harness_session_id"].is_string() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    store_secrets(&td).await;

    // The harness writes the file: the daemon never stored it.
    let file = common::claude_project_dir(&td.state, &id).join(format!(
        "{}.jsonl",
        s["harness_session_id"].as_str().unwrap()
    ));
    let line = json!({"type": "user", "uuid": "u1", "timestamp": "2026-10-26T01:00:00.000Z",
                      "message": {"role": "user", "content": leaky("typed")}})
    .to_string()
        + "\n";
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, line).unwrap();

    let url = format!(
        "/api/agents/{id}/sessions/{}/transcript",
        s["id"].as_str().unwrap()
    );
    let r = td.get(&url).await;
    let text = r.ok()["blocks"][0]["text"].as_str().unwrap().to_owned();
    assert!(text.contains(EXPECTED), "{text}");
    let list = td.get(&format!("/api/agents/{id}/sessions")).await;
    assert_redacted("the session list", &list.ok().to_string());

    // A delete changes the next read of the same cached file.
    td.delete("/api/secrets/p/deploy").await;
    let r = td.get(&url).await;
    let text = r.ok()["blocks"][0]["text"].as_str().unwrap().to_owned();
    assert!(text.contains(&format!("deploy={PROJECT_VALUE}")), "{text}");
    assert!(text.contains("gh=[secret:gh]"), "{text}");
}

/// `GET /api/sessions/{id}/transcript` (the card's session link) goes
/// through the same page code: it is redacted, also when it reads the
/// archived copy after `agent rm`.
#[tokio::test]
async fn session_links_are_redacted_too() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let a = td.agent("p", "w", json!({})).await;
    let id = a["id"].as_str().unwrap().to_owned();
    td.post_empty(&format!("/api/agents/{id}/start")).await.ok();
    td.wait_status(&id, "idle").await;
    let mut s = Value::Null;
    for _ in 0..200 {
        let r = td.get(&format!("/api/agents/{id}/sessions")).await;
        s = r.ok()[0].clone();
        if s["harness_session_id"].is_string() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    store_secrets(&td).await;
    let file = common::claude_project_dir(&td.state, &id).join(format!(
        "{}.jsonl",
        s["harness_session_id"].as_str().unwrap()
    ));
    let line = json!({"type": "user", "uuid": "u1", "timestamp": "2026-10-26T01:00:00.000Z",
                      "message": {"role": "user", "content": leaky("typed")}})
    .to_string()
        + "\n";
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, line).unwrap();

    let url = format!("/api/sessions/{}/transcript", s["id"].as_str().unwrap());
    let r = td.get(&url).await;
    let text = r.ok()["blocks"][0]["text"].as_str().unwrap().to_owned();
    assert!(text.contains(EXPECTED), "{text}");

    let r = td.delete(&format!("/api/agents/{id}?confirm=true")).await;
    assert_eq!(r.status, 204, "{}", r.body);
    let r = td.get(&url).await;
    assert_redacted("the archived transcript", &r.ok().to_string());
    let text = r.ok()["blocks"][0]["text"].as_str().unwrap().to_owned();
    assert!(text.contains(EXPECTED), "{text}");
}

/// S4 (review 2026-10-29): a project_admin cannot store a value that
/// blinds the records of other projects: its own agent ULID, a record
/// word, a name of this instance. The audit and the events keep their keys
/// and identifiers.
#[tokio::test]
async fn a_project_admin_cannot_blind_records() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.project("q").await;
    let w = td
        .agent("p", "w", json!({"roles": ["project_admin"]}))
        .await;
    let wid = w["id"].as_str().unwrap().to_owned();
    let label = format!("agent:{wid}");
    for v in [
        wid.as_str(),
        label.as_str(),
        "operator",
        "grantee_label",
        "a-quite-long-name",
    ] {
        if v == "a-quite-long-name" {
            td.project(v).await;
        }
        let r = td
            .post_as(
                &wid,
                "/api/secrets",
                json!({"scope": "p", "name": "x", "kind": "generic",
                       "fields": {"v": v}, "secret_fields": ["v"]}),
            )
            .await;
        assert_eq!(r.status, 422, "{v}: {}", r.body);
    }
    // A weak token is refused; a weak login password is stored with a
    // warning, and it is not redacted.
    let r = td
        .post_as(
            &wid,
            "/api/secrets",
            json!({"scope": "p", "name": "t", "kind": "token", "fields": {"value": "hostname1"}}),
        )
        .await;
    assert_eq!(r.status, 422, "{}", r.body);
    let r = td
        .post_as(
            &wid,
            "/api/secrets",
            json!({"scope": "p", "name": "win", "kind": "login",
                   "fields": {"username": "bob", "password": "summer2024"}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert!(
        r.body["warnings"][0].as_str().unwrap().contains("weak"),
        "{}",
        r.body
    );
    let m = td
        .post(
            "/api/mail",
            json!({"to": "p/w", "body": "summer2024 operator"}),
        )
        .await;
    assert_eq!(m.ok()["body"], "summer2024 operator");
    // The agent of `q` writes the value of a secret of `p`: the label has
    // no name (S5).
    td.post(
        "/api/secrets",
        json!({"scope": "p", "name": "deploy", "kind": "api_key",
               "fields": {"value": PROJECT_VALUE}}),
    )
    .await
    .ok();
    let b = td.agent("q", "b", json!({})).await;
    let bid = b["id"].as_str().unwrap().to_owned();
    let r = td
        .post_as(
            &bid,
            "/api/mail",
            json!({"to": "q/b", "body": format!("look {PROJECT_VALUE}")}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["body"], "look [secret]", "{}", r.body);
}
