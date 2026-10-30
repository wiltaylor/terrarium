//! The mailbox and project inboxes (PLAN §12).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::TestDaemon;
use serde_json::{Value, json};

async fn two_projects() -> TestDaemon {
    let td = TestDaemon::start().await;
    td.project("a").await;
    td.project("b").await;
    td.agent("a", "worker", json!({})).await;
    td.agent("a", "peer", json!({})).await;
    td.agent("a", "picky", json!({"policy": {"mail_to": ["peer"]}}))
        .await;
    td.agent(
        "a",
        "bridge",
        json!({"policy": {"mail_to": ["*"], "cross_project": ["b/dev"]}}),
    )
    .await;
    td.agent("b", "lead", json!({"roles": ["project_admin"]}))
        .await;
    td.agent("b", "dev", json!({})).await;
    td
}

#[tokio::test]
async fn mail_scope_rules() {
    let td = two_projects().await;
    // `*` is the same project.
    let m = td
        .post_as(
            "a/worker",
            "/api/mail",
            json!({"to": "peer", "subject": "hi", "body": "hello"}),
        )
        .await;
    let m = m.ok();
    assert_eq!(m["state"], "queued");
    assert_eq!(m["to_kind"], "agent");
    assert_eq!(m["to_name"], "a/peer");
    assert_eq!(m["from_name"], "a/worker");

    // Another project needs cross_project.
    let refused = td
        .post_as(
            "a/worker",
            "/api/mail",
            json!({"to": "b/dev", "body": "hello"}),
        )
        .await;
    refused.err(403, "forbidden");
    assert!(
        refused.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("cross_project")
    );
    td.post_as(
        "a/bridge",
        "/api/mail",
        json!({"to": "b/dev", "body": "allowed"}),
    )
    .await
    .ok();
    // A named mail_to list.
    td.post_as("a/picky", "/api/mail", json!({"to": "worker", "body": "x"}))
        .await
        .err(403, "forbidden");
    td.post_as("a/picky", "/api/mail", json!({"to": "peer", "body": "x"}))
        .await
        .ok();
    // The operator and project inboxes take mail from anyone.
    let op = td
        .post_as(
            "a/picky",
            "/api/mail",
            json!({"to": "operator", "body": "x"}),
        )
        .await;
    assert_eq!(op.ok()["state"], "delivered");
    // An agent cannot send as someone else.
    td.post_as(
        "a/worker",
        "/api/mail",
        json!({"from": "a/peer", "to": "operator", "body": "x"}),
    )
    .await
    .err(403, "forbidden");

    // An agent sees only its own mail.
    let peer_mail = td.mail_of("a/peer").await;
    assert_eq!(peer_mail.len(), 2);
    let id = peer_mail[0]["id"].as_str().unwrap();
    td.get_as("a/picky", &format!("/api/mail/{id}"))
        .await
        .err(404, "not_found");
    // Only the recipient marks it read.
    td.post_empty_as("a/worker", &format!("/api/mail/{id}/read"))
        .await
        .err(403, "forbidden");
    let read = td
        .post_empty_as("a/peer", &format!("/api/mail/{id}/read"))
        .await;
    assert_eq!(read.ok()["state"], "read");
    let unread = td.get_as("a/peer", "/api/mail?state=unread").await;
    assert_eq!(unread.ok().as_array().unwrap().len(), 1);

    // The operator sees everything and filters.
    let all = td.get("/api/mail?to=a%2Fpeer").await;
    assert_eq!(all.ok().as_array().unwrap().len(), 2);
    let ops = td.get("/api/mail?to=operator").await;
    assert_eq!(ops.ok().as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn replies_go_to_every_participant() {
    let td = two_projects().await;
    let first = td
        .post(
            "/api/mail",
            json!({"to": "a/worker", "subject": "plan", "body": "start"}),
        )
        .await
        .ok()
        .clone();
    let thread = first["thread_id"].as_str().unwrap().to_owned();
    // The worker brings the peer into the thread.
    td.post_as(
        "a/worker",
        "/api/mail",
        json!({"to": "peer", "body": "fyi", "thread_id": thread}),
    )
    .await
    .ok();
    // The peer replies: the operator and the worker get it.
    let peer_msg = td.mail_of("a/peer").await[0].clone();
    let replies = td
        .post_as(
            "a/peer",
            &format!("/api/mail/{}/reply", peer_msg["id"].as_str().unwrap()),
            json!({"body": "ack"}),
        )
        .await;
    let replies = replies.ok().as_array().unwrap().clone();
    let mut to: Vec<&str> = replies
        .iter()
        .map(|m| m["to_name"].as_str().unwrap())
        .collect();
    to.sort();
    assert_eq!(to, ["a/worker", "operator"]);
    assert!(replies.iter().all(|m| m["subject"] == "Re: plan"));
    assert!(replies.iter().all(|m| m["thread_id"] == thread.as_str()));
    let orig = td
        .get(&format!("/api/mail/{}", peer_msg["id"].as_str().unwrap()))
        .await;
    assert_eq!(orig.ok()["state"], "replied");
    // A non-participant cannot post on the thread.
    td.post_as(
        "a/picky",
        "/api/mail",
        json!({"to": "peer", "body": "sneak", "thread_id": thread}),
    )
    .await
    .err(403, "forbidden");
    let thread_view = td.get(&format!("/api/mail?thread={thread}")).await;
    assert_eq!(thread_view.ok().as_array().unwrap().len(), 4);
    // Mail raises an event on the recipient's stream.
    let worker = td.get("/api/agents/a%2Fworker").await.ok().clone();
    let events = td
        .get(&format!(
            "/api/agents/{}/events?kind=mail",
            worker["id"].as_str().unwrap()
        ))
        .await;
    assert!(!events.ok().as_array().unwrap().is_empty());
}

#[tokio::test]
async fn project_inbox_flow() {
    let td = two_projects().await;
    // 1. A worker in project a writes to project b's inbox.
    let sent = td
        .post_as(
            "a/worker",
            "/api/mail",
            json!({"to": "project:b", "subject": "bug: parser", "body": "it crashes"}),
        )
        .await;
    let sent = sent.ok().clone();
    assert_eq!(sent["to_kind"], "project");
    assert_eq!(sent["state"], "delivered");
    let thread = sent["thread_id"].as_str().unwrap().to_owned();
    // ...and links the thread to its own card.
    let own = td
        .post_as(
            "a/worker",
            "/api/projects/a/cards",
            json!({"title": "Wait for the b parser fix"}),
        )
        .await
        .ok()
        .clone();
    let own_id = own["id"].as_str().unwrap().to_owned();
    td.post_as(
        "a/worker",
        &format!("/api/cards/{own_id}/links"),
        json!({"kind": "thread", "ref": thread}),
    )
    .await
    .ok();
    // The other way in: POST /api/projects/{name}/inbox.
    td.post_as(
        "a/peer",
        "/api/projects/b/inbox",
        json!({"subject": "second", "body": "also"}),
    )
    .await
    .ok();

    // Members of b do not read the inbox; the lead and the operator do.
    td.get_as("b/dev", "/api/projects/b/inbox")
        .await
        .err(403, "forbidden");
    td.get_as("a/worker", "/api/mail?to=project:b")
        .await
        .err(403, "forbidden");
    let inbox = td
        .get_as("b/lead", "/api/projects/b/inbox?unread=true")
        .await;
    assert_eq!(inbox.ok().as_array().unwrap().len(), 2);
    assert_eq!(
        td.get("/api/projects/b/inbox")
            .await
            .ok()
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // 2. The lead makes a card from the mail and replies on the thread.
    let mid = sent["id"].as_str().unwrap();
    let card = td
        .post_as("b/lead", "/api/projects/b/cards", json!({"from_mail": mid}))
        .await;
    let card = card.ok().clone();
    assert_eq!(card["title"], "bug: parser");
    assert_eq!(card["links"][0]["kind"], "thread");
    assert_eq!(card["links"][0]["ref"], thread.as_str());
    let card_id = card["id"].as_str().unwrap().to_owned();
    // A member of b cannot make a card from a message it cannot read.
    td.post_as("b/dev", "/api/projects/b/cards", json!({"from_mail": mid}))
        .await
        .err(404, "not_found");

    td.post_empty_as("b/lead", &format!("/api/projects/b/inbox/{mid}/read"))
        .await
        .ok();
    let replies = td
        .post_as(
            "b/lead",
            &format!("/api/projects/b/inbox/{mid}/reply"),
            json!({"body": format!("accepted as {card_id}")}),
        )
        .await;
    let replies = replies.ok().as_array().unwrap().clone();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["from"], "project:b");
    assert_eq!(replies[0]["to_name"], "a/worker");
    // The worker replies on the thread, though b is another project: the
    // thread makes it a participant.
    let got = td.mail_of("a/worker").await;
    let accepted = got
        .iter()
        .find(|m| m["from"] == "project:b")
        .expect("the reply reached the worker");
    td.post_as(
        "a/worker",
        &format!("/api/mail/{}/reply", accepted["id"].as_str().unwrap()),
        json!({"body": "thanks"}),
    )
    .await
    .ok();

    // 3. The card moves to done: the thread gets a notice.
    td.post_as(
        "b/lead",
        &format!("/api/cards/{card_id}/move"),
        json!({"column": "done", "comment": "fixed in abc123"}),
    )
    .await
    .ok();
    let got = td.mail_of("a/worker").await;
    let notice: Vec<&Value> = got
        .iter()
        .filter(|m| m["subject"].as_str().unwrap().starts_with("Done:"))
        .collect();
    assert_eq!(notice.len(), 1, "{got:#?}");
    assert_eq!(notice[0]["thread_id"], thread.as_str());
    assert!(
        notice[0]["body"]
            .as_str()
            .unwrap()
            .contains("fixed in abc123")
    );
    assert!(notice[0]["body"].as_str().unwrap().contains(&card_id));
    // The linked card in a shows the update.
    let own = td.get(&format!("/api/cards/{own_id}")).await;
    let comments = own.ok()["comments"].as_array().unwrap();
    assert_eq!(comments.last().unwrap()["author"], "project:b");
    assert!(
        comments.last().unwrap()["body"]
            .as_str()
            .unwrap()
            .contains(&card_id)
    );
}

/// The first read of a message records the reader (`read_by`) and writes
/// one `mail.read` audit row; a read again writes none. After `agent rm`,
/// the mail keeps the agent's name (migration 12).
#[tokio::test]
async fn mail_reads_are_audited_and_names_stay_after_agent_rm() {
    let td = two_projects().await;
    // The operator (the CLI or the dashboard) reads mail from an agent.
    let m = td
        .post_as(
            "a/worker",
            "/api/mail",
            json!({"to": "operator", "body": "done"}),
        )
        .await;
    let m = m.ok()["id"].as_str().unwrap().to_owned();
    let r = td.post_empty(&format!("/api/mail/{m}/read")).await;
    assert_eq!(r.ok()["read_by"], "operator");
    assert_eq!(r.ok()["read_by_name"], "operator");
    td.post_empty(&format!("/api/mail/{m}/read")).await.ok();
    let audit = td.get("/api/audit?action=mail.read").await;
    let rows = audit.ok().as_array().unwrap().clone();
    assert_eq!(rows.len(), 1, "{rows:#?}");
    assert_eq!(rows[0]["actor"], "operator");
    assert_eq!(rows[0]["target"], m.as_str());
    assert_eq!(rows[0]["detail"]["via"], "read");
    assert!(rows[0]["detail"]["thread"].is_string());

    // An agent reads its mail.
    let n = td
        .post("/api/mail", json!({"to": "a/peer", "body": "hi"}))
        .await;
    let n = n.ok()["id"].as_str().unwrap().to_owned();
    let peer = td.get("/api/agents/a%2Fpeer").await.ok()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let r = td
        .post_empty_as("a/peer", &format!("/api/mail/{n}/read"))
        .await;
    assert_eq!(r.ok()["read_by"], peer.as_str());
    assert_eq!(r.ok()["read_by_name"], "a/peer");
    let audit = td.get("/api/audit?action=mail.read").await;
    let rows = audit.ok().as_array().unwrap().clone();
    assert_eq!(rows.len(), 2, "{rows:#?}");
    assert_eq!(rows[1]["actor"], format!("agent:{peer}"));
    assert_eq!(rows[1]["project"], "a");

    // Mail to the worker, before it goes.
    let to_worker = td
        .post("/api/mail", json!({"to": "a/worker", "body": "your turn"}))
        .await;
    let to_worker = to_worker.ok()["id"].as_str().unwrap().to_owned();

    // The worker goes; its mail keeps the name. The mail woke it (PLAN
    // §18), so the delete needs `confirm`.
    let worker = td.get("/api/agents/a%2Fworker").await.ok()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let r = td
        .delete(&format!("/api/agents/{worker}?confirm=true"))
        .await;
    assert_eq!(r.status, 204, "{}", r.body);
    let got = td.get(&format!("/api/mail/{m}")).await;
    assert_eq!(got.ok()["from"], worker.as_str());
    assert_eq!(got.ok()["from_name"], "a/worker");
    // The list shows the name too, for mail from and to the worker.
    let all = td.get("/api/mail").await;
    let all = all.ok().as_array().unwrap().clone();
    let sent = all.iter().find(|x| x["id"] == m.as_str()).unwrap();
    assert_eq!(sent["from_name"], "a/worker", "{sent}");
    let got = all.iter().find(|x| x["id"] == to_worker.as_str()).unwrap();
    assert_eq!(got["to"], worker.as_str(), "{got}");
    assert_eq!(got["to_name"], "a/worker", "{got}");
    // A thread read shows the name too.
    let to = td
        .get(&format!(
            "/api/mail?thread={}",
            got["thread_id"].as_str().unwrap()
        ))
        .await;
    assert_eq!(to.ok()[0]["to_name"], "a/worker");
    // The list filters still take the removed agent, by ULID and by
    // project/name, through the tombstone.
    for key in [worker.as_str(), "a/worker", "a%2Fworker"] {
        let by_to = td.get(&format!("/api/mail?to={key}")).await;
        let by_to = by_to.ok().as_array().unwrap().clone();
        assert_eq!(by_to.len(), 1, "to={key}: {by_to:#?}");
        assert_eq!(by_to[0]["id"], to_worker.as_str());
        assert_eq!(by_to[0]["to_name"], "a/worker");
        let by_from = td.get(&format!("/api/mail?from={key}")).await;
        let by_from = by_from.ok().as_array().unwrap().clone();
        assert_eq!(by_from.len(), 1, "from={key}: {by_from:#?}");
        assert_eq!(by_from[0]["id"], m.as_str());
        assert_eq!(by_from[0]["from_name"], "a/worker");
    }
    // Mail to the removed agent stays refused.
    let r = td
        .post("/api/mail", json!({"to": worker, "body": "late"}))
        .await;
    assert_eq!(r.status, 404, "{}", r.body);
}

/// A list filter key that never was an agent is not found, also with the
/// tombstone fallback of the list filters.
#[tokio::test]
async fn mail_list_filter_on_a_never_existing_agent_is_not_found() {
    let td = two_projects().await;
    for q in [
        "to=a/nobody",
        "from=a/nobody",
        "to=nobody",
        "to=01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "from=01ARZ3NDEKTSV4RRFFQ69G5FAV",
    ] {
        let r = td.get(&format!("/api/mail?{q}")).await;
        assert_eq!(r.status, 404, "{q}: {}", r.body);
    }
}

/// The `from` and `to` filters of `GET /api/mail` take the same address
/// kinds as the send path: `operator`, `system`, `project:<name>`, and
/// agents. Mail to `system` and mail as `system` stay refused.
#[tokio::test]
async fn mail_list_filters_take_operator_system_and_project() {
    let td = two_projects().await;
    // A blocked card: the daemon (`system`) mails the operator.
    let card = td
        .post_as(
            "a/worker",
            "/api/projects/a/cards",
            json!({"title": "stuck"}),
        )
        .await
        .ok()
        .clone();
    td.post_as(
        "a/worker",
        &format!("/api/cards/{}/move", card["id"].as_str().unwrap()),
        json!({"column": "blocked", "comment": "need a key"}),
    )
    .await
    .ok();
    td.post_as(
        "a/worker",
        "/api/mail",
        json!({"to": "project:b", "body": "for b"}),
    )
    .await
    .ok();
    td.post_as(
        "a/worker",
        "/api/mail",
        json!({"to": "operator", "body": "hi"}),
    )
    .await
    .ok();

    let from_system = td.get("/api/mail?from=system").await;
    let rows = from_system.ok().as_array().unwrap().clone();
    assert!(!rows.is_empty(), "no mail from system");
    assert!(rows.iter().all(|m| m["from"] == "system"), "{rows:#?}");
    assert!(
        rows.iter()
            .any(|m| m["subject"].as_str().unwrap().contains("blocked")),
        "{rows:#?}"
    );
    // Nothing goes to `system`: an empty list, not a 404.
    let to_system = td.get("/api/mail?to=system").await;
    assert_eq!(to_system.ok().as_array().unwrap().len(), 0);
    let to_operator = td.get("/api/mail?to=operator").await;
    let rows = to_operator.ok().as_array().unwrap().clone();
    assert!(rows.iter().all(|m| m["to"] == "operator"), "{rows:#?}");
    assert!(rows.iter().any(|m| m["from"] == "system"), "{rows:#?}");
    assert!(rows.iter().any(|m| m["body"] == "hi"), "{rows:#?}");
    let to_b = td.get("/api/mail?to=project:b").await;
    let rows = to_b.ok().as_array().unwrap().clone();
    assert_eq!(rows.len(), 1, "{rows:#?}");
    assert_eq!(rows[0]["to"], "project:b");
    let from_op = td.get("/api/mail?from=operator").await;
    assert_eq!(from_op.ok().as_array().unwrap().len(), 0);
    td.get("/api/mail?to=project:nope")
        .await
        .err(404, "not_found");

    // The send path still refuses `system` both ways.
    td.post("/api/mail", json!({"to": "system", "body": "x"}))
        .await
        .err(422, "invalid");
    td.post(
        "/api/mail",
        json!({"from": "system", "to": "operator", "body": "x"}),
    )
    .await
    .err(403, "forbidden");
}
