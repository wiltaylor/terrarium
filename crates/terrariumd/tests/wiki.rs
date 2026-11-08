//! The wiki and its rules (PLAN §15.1).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::TestDaemon;
use hyper::Method;
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
    td.agent("proj", "w2", json!({})).await;
    td.project("other").await;
    td.agent("other", "x", json!({})).await;
    td
}

const DOC: &str =
    "# Runner v2\n\nIntro.\n\n## Design\n\nOne process per agent.\n\n## Risks\n\nNone yet.\n";

async fn page(td: &TestDaemon, agent: &str, path: &str, body: &str) -> Value {
    td.post_as(
        agent,
        "/api/projects/proj/wiki",
        json!({"path": path, "body": body, "summary": "First draft"}),
    )
    .await
    .ok()
    .clone()
}

fn paths(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|p| p["path"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn index_on_first_access_and_create() {
    let td = setup().await;
    let r = td.get_as("proj/w1", "/api/projects/proj/wiki").await;
    assert_eq!(paths(r.ok()), ["index"]);
    // The other project gets its index when someone opens it.
    let r = td.get("/api/wiki/other%2Findex").await;
    assert_eq!(r.ok()["path"], "index");
    assert_eq!(r.ok()["author"], "system");
    // A bare path is the agent's own project.
    let r = td.get_as("proj/w1", "/api/wiki/index").await;
    assert_eq!(r.ok()["project"], "proj");

    let p = page(&td, "proj/w1", "designs/runner-v2", DOC).await;
    assert_eq!(p["rev"], 1);
    assert_eq!(p["title"], "Runner v2");
    assert_eq!(p["author_name"], "proj/w1");
    td.post_as(
        "proj/w1",
        "/api/projects/proj/wiki",
        json!({"path": "designs/runner-v2", "body": "x", "summary": "dup"}),
    )
    .await
    .err(409, "conflict");
    td.post_as(
        "proj/w1",
        "/api/projects/proj/wiki",
        json!({"path": "a", "body": "x", "summary": "  "}),
    )
    .await
    .err(422, "invalid");
    td.post_as(
        "proj/w1",
        "/api/projects/proj/wiki",
        json!({"path": "Bad Path", "body": "x", "summary": "s"}),
    )
    .await
    .err(422, "invalid");
    td.post_as(
        "other/x",
        "/api/projects/proj/wiki",
        json!({"path": "a", "body": "x", "summary": "s"}),
    )
    .await
    .err(403, "forbidden");
    let r = td.get_as("proj/w2", "/api/projects/proj/wiki").await;
    assert_eq!(paths(r.ok()), ["designs/runner-v2", "index"]);
    // Search over the current revision.
    let r = td
        .get_as("proj/w2", "/api/projects/proj/wiki?q=process")
        .await;
    let hits = r.ok().as_array().unwrap().clone();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["path"], "designs/runner-v2");
    assert!(hits[0]["snippet"].as_str().unwrap().contains("process"));
    let ev = td.get("/api/events?kind=wiki.created").await;
    assert_eq!(ev.ok().as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn stale_base_is_refused_with_a_diff_and_sections_do_not_conflict() {
    let td = setup().await;
    let p = page(&td, "proj/w1", "designs/runner-v2", DOC).await;
    let id = p["id"].as_str().unwrap().to_owned();
    let url = format!("/api/wiki/{id}");

    // w2 saves on rev 1.
    let body2 = DOC.replace("None yet.", "PTY leaks.");
    let r = td
        .put_as(
            "proj/w2",
            &url,
            json!({"body": body2, "base": 1, "summary": "Add a risk"}),
        )
        .await;
    assert_eq!(r.ok()["rev"], 2);
    // w1 still has rev 1: refused, with the diff since rev 1.
    let r = td
        .put_as(
            "proj/w1",
            &url,
            json!({"body": DOC.replace("Intro.", "Hello."), "base": 1, "summary": "Edit intro"}),
        )
        .await;
    r.err(409, "stale_base");
    let d = &r.body["error"]["details"];
    assert_eq!(d["base"], 1);
    assert_eq!(d["current"], 2);
    let diff = d["diff"].as_str().unwrap();
    assert!(diff.contains("-None yet."), "{diff}");
    assert!(diff.contains("+PTY leaks."), "{diff}");
    td.put_as(
        "proj/w1",
        &url,
        json!({"body": "x", "base": 2, "summary": ""}),
    )
    .await
    .err(422, "invalid");

    // Section edits from the same base touch different sections: no conflict.
    let r = td
        .post_as(
            "proj/w1",
            &format!("{url}/section"),
            json!({"heading": "Design", "body": "Two processes.", "base": 2, "summary": "Design"}),
        )
        .await;
    assert_eq!(r.ok()["rev"], 3);
    let r = td
        .post_as(
            "proj/w2",
            &format!("{url}/section"),
            json!({"heading": "## Risks", "body": "PTY leaks; OOM.", "base": 2, "summary": "Risks"}),
        )
        .await;
    let body = r.ok()["body"].as_str().unwrap().to_owned();
    assert_eq!(r.ok()["rev"], 4);
    assert!(
        body.contains("## Design\n\nTwo processes.\n\n## Risks\n\nPTY leaks; OOM.\n"),
        "{body}"
    );
    // The same section from a stale base does conflict.
    let r = td
        .post_as(
            "proj/w2",
            &format!("{url}/section"),
            json!({"heading": "Design", "body": "Three.", "base": 2, "summary": "Design"}),
        )
        .await;
    r.err(409, "stale_base");
    assert!(
        r.body["error"]["details"]["diff"]
            .as_str()
            .unwrap()
            .contains("+Two processes.")
    );
    // A new heading is added at the end; append never conflicts.
    td.post_as(
        "proj/w2",
        &format!("{url}/section"),
        json!({"heading": "Open questions", "body": "Cgroups?", "summary": "Q"}),
    )
    .await
    .ok();
    let r = td
        .post_as(
            "proj/w1",
            &format!("{url}/append"),
            json!({"body": "Signed off.", "summary": "Sign off"}),
        )
        .await;
    let body = r.ok()["body"].as_str().unwrap();
    assert!(
        body.ends_with("## Open questions\n\nCgroups?\n\nSigned off.\n"),
        "{body}"
    );
    assert_eq!(r.ok()["rev"], 6);

    // History, one revision, and a diff.
    let revs = td.get(&format!("{url}/revisions")).await;
    let revs = revs.ok().as_array().unwrap().clone();
    assert_eq!(revs.len(), 6);
    assert_eq!(revs[0]["rev"], 6);
    assert!(revs[0].get("body").is_none());
    let r1 = td.get(&format!("{url}/revisions/1")).await;
    assert_eq!(r1.ok()["body"], DOC);
    let d = td.get(&format!("{url}/diff?a=1&b=2")).await;
    assert!(d.ok()["diff"].as_str().unwrap().contains("+PTY leaks."));
}

#[tokio::test]
async fn backlinks_delete_restore_move() {
    let td = setup().await;
    let target = page(&td, "proj/w1", "designs/runner-v2", DOC).await;
    let tid = target["id"].as_str().unwrap().to_owned();
    td.post_as("proj/w1", "/api/projects/proj/cards", json!({"title": "c"}))
        .await
        .ok();
    let r = page(
        &td,
        "proj/w2",
        "reports/week-3",
        "See [[designs/runner-v2|the design]], [[TER-1]], [[MEM-4]] and ![x](artifact:ART-2).\n",
    )
    .await;
    let links = r["links"].as_array().unwrap();
    assert!(links.contains(&json!({"kind": "card", "target": "TER-1"})));
    assert!(links.contains(&json!({"kind": "artifact", "target": "proj/ART-2"})));
    let b = td.get(&format!("/api/wiki/{tid}/backlinks")).await;
    assert_eq!(paths(b.ok()), ["reports/week-3"]);

    // Only admins delete; history stays; restore brings it back.
    let rid = r["id"].as_str().unwrap().to_owned();
    td.req(
        Method::DELETE,
        &format!("/api/wiki/{rid}"),
        None,
        Some("proj/w2"),
    )
    .await
    .err(403, "forbidden");
    let d = td
        .req(
            Method::DELETE,
            &format!("/api/wiki/{rid}"),
            None,
            Some("proj/lead"),
        )
        .await;
    assert_eq!(d.status.as_u16(), 204, "{}", d.body);
    td.get_as("proj/w1", "/api/wiki/reports%2Fweek-3")
        .await
        .err(404, "not_found");
    let b = td.get(&format!("/api/wiki/{tid}/backlinks")).await;
    assert_eq!(b.ok(), &json!([]));
    let r = td.get(&format!("/api/wiki/{rid}")).await;
    assert!(r.ok()["deleted_at"].is_string());
    let r = td.get("/api/projects/proj/wiki?deleted=true").await;
    assert_eq!(paths(r.ok()), ["reports/week-3"]);
    td.post_as(
        "proj/w2",
        &format!("/api/wiki/{rid}/restore"),
        json!({"rev": 1}),
    )
    .await
    .err(403, "forbidden");
    let r = td
        .post_as(
            "proj/lead",
            &format!("/api/wiki/{rid}/restore"),
            json!({"rev": 1}),
        )
        .await;
    assert_eq!(r.ok()["rev"], 2);
    assert!(r.ok().get("deleted_at").is_none());
    let audit = td.get("/api/audit?action=wiki.delete").await;
    assert_eq!(audit.ok().as_array().unwrap().len(), 1);

    // A member restores an old revision of a live page.
    let r = td
        .post_as(
            "proj/w1",
            &format!("/api/wiki/{rid}/restore"),
            json!({"rev": 1}),
        )
        .await;
    assert_eq!(r.ok()["rev"], 3);

    // Only admins move.
    td.post_as(
        "proj/w1",
        &format!("/api/wiki/{tid}/move"),
        json!({"path": "designs/runner"}),
    )
    .await
    .err(403, "forbidden");
    let r = td
        .post_as(
            "proj/lead",
            &format!("/api/wiki/{tid}/move"),
            json!({"path": "designs/runner"}),
        )
        .await;
    assert_eq!(r.ok()["path"], "designs/runner");
    td.get_as("proj/w1", "/api/wiki/designs%2Frunner")
        .await
        .ok();
}

#[tokio::test]
async fn review_marks_reviewed_and_mails_the_author() {
    let td = setup().await;
    let body = "---\nstatus: ready_for_review\nreviewer: lead\n---\n# Perf report\n\nFaster.\n";
    let p = page(&td, "proj/w1", "reports/perf", body).await;
    assert_eq!(p["status"], "ready_for_review");
    assert_eq!(p["reviewer"], "lead");
    let id = p["id"].as_str().unwrap().to_owned();
    let r = td
        .get("/api/projects/proj/wiki?status=ready_for_review")
        .await;
    assert_eq!(paths(r.ok()), ["reports/perf"]);
    td.post(
        "/api/projects/proj/wiki",
        json!({"path": "bad", "body": "---\nstatus: done\n---\n", "summary": "s"}),
    )
    .await
    .err(422, "invalid");

    // Not the reviewer, not an admin.
    td.post_as(
        "proj/w2",
        &format!("/api/wiki/{id}/review"),
        json!({"decision": "approve"}),
    )
    .await
    .err(403, "forbidden");
    td.post_as(
        "proj/lead",
        &format!("/api/wiki/{id}/review"),
        json!({"decision": "comment"}),
    )
    .await
    .err(422, "comment_required");
    let r = td
        .post_as(
            "proj/lead",
            &format!("/api/wiki/{id}/review"),
            json!({"decision": "approve", "comment": "Looks good."}),
        )
        .await;
    let r = r.ok();
    assert_eq!(r["status"], "reviewed");
    assert_eq!(r["rev"], 2);
    assert!(
        r["body"]
            .as_str()
            .unwrap()
            .starts_with("---\nstatus: reviewed\n")
    );
    let mail = td.mail_of("proj/w1").await;
    assert_eq!(mail.len(), 1);
    assert_eq!(mail[0]["subject"], "Reviewed: reports/perf");
    assert_eq!(mail[0]["from"], "project:proj");
    assert!(mail[0]["body"].as_str().unwrap().contains("Looks good."));
    let ev = td.get("/api/events?kind=wiki.reviewed").await;
    assert_eq!(ev.ok().as_array().unwrap().len(), 1);
}
