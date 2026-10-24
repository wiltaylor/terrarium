//! Versions: the review flow, immutability, wait, and compare (PLAN §15.3).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use base64::Engine;
use common::TestDaemon;
use serde_json::{Value, json};

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
    td.project("other").await;
    td.agent("other", "x", json!({})).await;
    td
}

async fn card(td: &TestDaemon, title: &str) -> String {
    td.post_as(
        "proj/w1",
        "/api/projects/proj/cards",
        json!({"title": title}),
    )
    .await
    .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn comments(card: &Value) -> Vec<String> {
    card["comments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["body"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn review_flow_mails_and_comments_and_release_is_immutable() {
    let td = setup().await;
    let c1 = card(&td, "Parser").await;
    let c2 = card(&td, "Lexer").await;
    let data = base64::engine::general_purpose::STANDARD.encode(b"report");
    td.post_as(
        "proj/lead",
        "/api/projects/proj/artifacts",
        json!({"name": "report.txt", "data_base64": data}),
    )
    .await
    .ok();
    let v = td
        .post_as(
            "proj/lead",
            "/api/projects/proj/versions",
            json!({"name": "v0.1.0", "notes": "First cut.", "git_ref": "abc123",
                   "cards": [c1], "artifacts": ["ART-1"]}),
        )
        .await;
    let v = v.ok().clone();
    assert_eq!(v["status"], "draft");
    assert_eq!(v["cards"][0]["id"], "TER-1");
    assert_eq!(v["artifacts"][0]["id"], "ART-1");
    let id = v["id"].as_str().unwrap().to_owned();
    let url = format!("/api/versions/{id}");
    td.post_as(
        "proj/lead",
        "/api/projects/proj/versions",
        json!({"name": "v0.1.0"}),
    )
    .await
    .err(409, "conflict");
    td.post_as(
        "proj/lead",
        "/api/projects/proj/versions",
        json!({"name": "bad name"}),
    )
    .await
    .err(422, "invalid");
    td.get_as("other/x", &url).await.err(403, "forbidden");
    // Add a card by name, as the creator.
    let r = td
        .patch_as(
            "proj/lead",
            "/api/versions/v0.1.0",
            json!({"add_cards": [c2]}),
        )
        .await;
    assert_eq!(r.ok()["cards"].as_array().unwrap().len(), 2);
    td.patch_as("proj/w1", &url, json!({"notes": "mine"}))
        .await
        .err(403, "forbidden");

    // Submit: in_review, and the operator gets mail with the link.
    td.post_empty_as("proj/w1", &format!("{url}/submit"))
        .await
        .err(403, "forbidden");
    let r = td
        .post_as(
            "proj/lead",
            &format!("{url}/submit"),
            json!({"comment": "Please look."}),
        )
        .await;
    assert_eq!(r.ok()["status"], "in_review");
    let thread = r.ok()["thread_id"].as_str().unwrap().to_owned();
    let mail = td.get("/api/mail?to=operator").await;
    let mail = mail.ok().as_array().unwrap().clone();
    assert_eq!(mail.len(), 1);
    assert_eq!(mail[0]["subject"], "Review version v0.1.0 of proj");
    assert_eq!(mail[0]["from"], "project:proj");
    let body = mail[0]["body"].as_str().unwrap();
    // The link opens the version in the dashboard (TERRARIUM_PUBLIC_URL,
    // default http://localhost:8080).
    assert!(
        body.contains(&format!("Open: http://localhost:8080/p/proj/versions/{id}")),
        "{body}"
    );
    assert!(body.contains("TER-1 Parser"), "{body}");
    // Edits wait for the decision.
    td.patch_as("proj/lead", &url, json!({"notes": "x"}))
        .await
        .err(409, "conflict");

    // Only the operator decides; a rejection needs a comment.
    td.post_as("proj/lead", &format!("{url}/approve"), json!({}))
        .await
        .err(403, "forbidden");
    td.post_empty(&format!("{url}/reject"))
        .await
        .err(422, "comment_required");
    let r = td
        .post(
            &format!("{url}/reject"),
            json!({"comment": "Tests fail on arm64."}),
        )
        .await;
    assert_eq!(r.ok()["status"], "rejected");
    let lead_mail = td.mail_of("proj/lead").await;
    assert_eq!(lead_mail.len(), 1);
    assert_eq!(lead_mail[0]["subject"], "Version v0.1.0 rejected");
    assert_eq!(lead_mail[0]["thread_id"], thread.as_str());
    assert!(lead_mail[0]["body"].as_str().unwrap().contains("arm64"));
    for c in [&c1, &c2] {
        let card = td.get(&format!("/api/cards/{c}")).await;
        let cs = comments(card.ok());
        assert!(
            cs.iter()
                .any(|b| b.contains("v0.1.0 rejected by operator: Tests fail on arm64.")),
            "{cs:?}"
        );
    }

    // Fix and submit again, on the same thread; approve.
    td.patch_as("proj/lead", &url, json!({"notes": "Fixed arm64."}))
        .await
        .ok();
    let r = td
        .post_empty_as("proj/lead", &format!("{url}/submit"))
        .await;
    assert_eq!(r.ok()["thread_id"], thread.as_str());
    let r = td
        .post(&format!("{url}/approve"), json!({"comment": "Ship it."}))
        .await;
    let r = r.ok();
    assert_eq!(r["status"], "approved");
    assert_eq!(r["decided_by"], "operator");
    let decisions: Vec<&str> = r["reviews"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["decision"].as_str().unwrap())
        .collect();
    assert_eq!(
        decisions,
        ["submitted", "rejected", "submitted", "approved"]
    );
    let lead_mail = td.mail_of("proj/lead").await;
    assert_eq!(lead_mail.len(), 2);
    assert_eq!(lead_mail[1]["subject"], "Version v0.1.0 approved");
    let card = td.get(&format!("/api/cards/{c1}")).await;
    assert!(
        comments(card.ok())
            .iter()
            .any(|b| b.contains("approved by operator: Ship it."))
    );

    // Release: admins only, from approved only.
    td.post_empty_as("proj/w1", &format!("{url}/release"))
        .await
        .err(403, "forbidden");
    let r = td
        .post_empty_as("proj/lead", &format!("{url}/release"))
        .await;
    assert_eq!(r.ok()["status"], "released");
    let audit = td.get("/api/audit?action=version.release").await;
    assert_eq!(audit.ok().as_array().unwrap().len(), 1);
    let audit = td.get("/api/audit?action=version.approve").await;
    assert_eq!(audit.ok().as_array().unwrap().len(), 1);

    // Released is immutable.
    td.patch_as("proj/lead", &url, json!({"notes": "late"}))
        .await
        .err(409, "conflict");
    td.patch(&url, json!({"notes": "late"}))
        .await
        .err(409, "conflict");
    td.post_empty_as("proj/lead", &format!("{url}/submit"))
        .await
        .err(409, "conflict");
    td.post_empty(&format!("{url}/approve"))
        .await
        .err(409, "conflict");
    td.post_empty(&format!("{url}/reject"))
        .await
        .err(409, "conflict");
    td.post_empty_as("proj/lead", &format!("{url}/release"))
        .await
        .err(409, "conflict");
    td.req(
        hyper::Method::DELETE,
        "/api/artifacts/ART-1",
        None,
        Some("proj/lead"),
    )
    .await
    .err(409, "conflict");

    let kinds: Vec<String> = td
        .get("/api/events?project=proj")
        .await
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_owned())
        .filter(|k| k.starts_with("version."))
        .collect();
    for k in [
        "version.created",
        "version.updated",
        "version.submitted",
        "version.rejected",
        "version.approved",
        "version.released",
    ] {
        assert!(kinds.iter().any(|x| x == k), "{k} not in {kinds:?}");
    }
}

#[tokio::test]
async fn wait_wakes_on_a_decision() {
    let td = std::sync::Arc::new(setup().await);
    td.post_as(
        "proj/lead",
        "/api/projects/proj/versions",
        json!({"name": "v1"}),
    )
    .await
    .ok();
    td.post_empty_as("proj/lead", "/api/versions/v1/submit")
        .await
        .ok();
    let r = td
        .get_as("proj/lead", "/api/projects/proj/versions?status=approved")
        .await;
    assert_eq!(r.ok(), &json!([]));
    let waiter = {
        let td = td.clone();
        tokio::spawn(async move {
            td.get_as(
                "proj/lead",
                "/api/projects/proj/versions?status=approved&wait=10",
            )
            .await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    td.post_empty("/api/versions/proj%2Fv1/approve").await.ok();
    let r = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
        .await
        .unwrap()
        .unwrap();
    let list = r.ok().as_array().unwrap().clone();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["name"], "v1");
}

#[tokio::test]
async fn compare_reports_cards_pages_and_artifacts() {
    let td = setup().await;
    let c1 = card(&td, "One").await;
    let page = td
        .post_as(
            "proj/w1",
            "/api/projects/proj/wiki",
            json!({"path": "notes", "body": "v1 notes", "summary": "s"}),
        )
        .await;
    let page_id = page.ok()["id"].as_str().unwrap().to_owned();
    td.post_as(
        "proj/lead",
        "/api/projects/proj/versions",
        json!({"name": "v1", "cards": [c1]}),
    )
    .await
    .ok();
    td.post_empty_as("proj/lead", "/api/versions/v1/submit")
        .await
        .ok();
    td.post_empty("/api/versions/proj%2Fv1/approve").await.ok();
    td.post_empty_as("proj/lead", "/api/versions/v1/release")
        .await
        .ok();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    // After v1: a card lands, a page changes, a page appears.
    let c2 = card(&td, "Two").await;
    td.post_empty_as("proj/w1", &format!("/api/cards/{c2}/claim"))
        .await
        .ok();
    td.post_as(
        "proj/w1",
        &format!("/api/cards/{c2}/move"),
        json!({"column": "done", "comment": "Merged."}),
    )
    .await
    .ok();
    td.put_as(
        "proj/w1",
        &format!("/api/wiki/{page_id}"),
        json!({"body": "v2 notes", "base": 1, "summary": "s"}),
    )
    .await
    .ok();
    td.post_as(
        "proj/w1",
        "/api/projects/proj/wiki",
        json!({"path": "new-page", "body": "x", "summary": "s"}),
    )
    .await
    .ok();
    let data = base64::engine::general_purpose::STANDARD.encode(b"build");
    td.post_as(
        "proj/lead",
        "/api/projects/proj/artifacts",
        json!({"name": "build.bin", "data_base64": data}),
    )
    .await
    .ok();
    td.post_as(
        "proj/lead",
        "/api/projects/proj/versions",
        json!({"name": "v2", "cards": [c2], "artifacts": ["ART-1"]}),
    )
    .await
    .ok();
    let r = td.get("/api/versions/proj%2Fv2/compare?to=v1").await;
    let r = r.ok();
    assert_eq!(r["version"], "v2");
    assert_eq!(r["to"], "v1");
    assert_eq!(r["cards"]["added"], json!([c2]));
    assert_eq!(r["cards"]["removed"], json!([c1]));
    assert_eq!(r["cards"]["landed"], json!([c2]));
    assert_eq!(r["artifacts"]["added"], json!(["ART-1"]));
    let pages: Vec<(String, Value, Value)> = r["pages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["path"].as_str().unwrap().to_owned(),
                p["from_rev"].clone(),
                p["to_rev"].clone(),
            )
        })
        .collect();
    assert!(
        pages.contains(&("notes".into(), json!(1), json!(2))),
        "{pages:?}"
    );
    assert!(
        pages.contains(&("new-page".into(), Value::Null, json!(1))),
        "{pages:?}"
    );
    assert!(!pages.iter().any(|p| p.0 == "index"), "{pages:?}");
    td.get("/api/versions/proj%2Fv2/compare")
        .await
        .err(400, "bad_request");
}

/// `Version.url` is the dashboard route `/p/<project>/versions/<id>`: a
/// path by default, absolute with TERRARIUM_PUBLIC_URL, the same link as
/// the review mail.
#[tokio::test]
async fn version_url_is_the_dashboard_route() {
    let mut td = setup().await;
    let v = td
        .post_as(
            "proj/lead",
            "/api/projects/proj/versions",
            json!({"name": "v1"}),
        )
        .await
        .ok()
        .clone();
    let id = v["id"].as_str().unwrap().to_owned();
    assert_eq!(v["url"], format!("/p/proj/versions/{id}"));

    td.web.public_url = Some("https://t.example.com".into());
    td.boot(None, None).await;
    let want = format!("https://t.example.com/p/proj/versions/{id}");
    let got = td.get(&format!("/api/versions/{id}")).await;
    assert_eq!(got.ok()["url"], want);
    let list = td.get("/api/projects/proj/versions").await;
    assert_eq!(list.ok()[0]["url"], want);
    let sub = td
        .post_empty_as("proj/lead", &format!("/api/versions/{id}/submit"))
        .await;
    assert_eq!(sub.ok()["url"], want);
    // The review mail names the same page.
    let mail = td.get("/api/mail?to=operator").await;
    let body = mail.ok()[0]["body"].as_str().unwrap().to_owned();
    assert!(body.contains(&format!("Open: {want}")), "{body}");
}
