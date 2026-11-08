//! D5: the question guard (`terrariumd::question_guard`). A worker turn
//! that ends with a question in plain text and reports nothing makes the
//! agent `blocked` and mails the lead (or the operator). Leads are exempt;
//! a turn that sent mail or set `blocked` is left alone. The transcript is
//! a fake: the test gives the last text.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::TestDaemon;
use serde_json::{Value, json};
use terrarium_core::{AgentId, Event};

async fn running(td: &TestDaemon, project: &str, name: &str, def: Value) -> (String, AgentId) {
    let a = td.agent(project, name, def).await;
    let id = a["id"].as_str().unwrap().to_owned();
    td.post_empty(&format!("/api/agents/{id}/start")).await;
    td.wait_status(&id, "idle").await;
    let aid = id.parse().unwrap();
    (id, aid)
}

/// The seq of the newest event of `kind` of agent `id`, once it is stored.
async fn seq_of(td: &TestDaemon, id: &str, kind: &str, after: i64) -> i64 {
    for _ in 0..200 {
        let r = td
            .get(&format!("/api/agents/{id}/events?kind={kind}"))
            .await;
        if let Some(s) = r
            .ok()
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["seq"].as_i64())
            .filter(|s| *s > after)
            .max()
        {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no {kind} event of {id}");
}

/// One turn of `agent`: `turn_start`, then `during`, then `turn_end`. The
/// seq of the `turn_end`.
async fn turn(td: &TestDaemon, id: &str, aid: AgentId, during: impl AsyncFnOnce()) -> i64 {
    let before = seq_of_any(td, id).await;
    td.sup.emit_harness(aid, Event::TurnStart { prompt: None });
    seq_of(td, id, "turn_start", before).await;
    during().await;
    td.sup.emit_harness(aid, Event::TurnEnd);
    seq_of(td, id, "turn_end", before).await
}

async fn seq_of_any(td: &TestDaemon, id: &str) -> i64 {
    td.get(&format!("/api/agents/{id}/events"))
        .await
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["seq"].as_i64())
        .max()
        .unwrap_or(0)
}

fn state(td: &TestDaemon) -> terrariumd::api::Shared {
    td.daemon.as_ref().unwrap().state.clone()
}

#[tokio::test]
async fn status_lines_do_not_block_or_mail_the_lead() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let lead = td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let (id, aid) = running(&td, "p", "w", json!({})).await;
    let end = turn(&td, &id, aid, async || {}).await;
    for text in [
        "Next: when you tell me TER-91 is merged, I merge ...",
        "I am idle now, as you asked. I am waiting for your mail that TER-91 is merged.",
    ] {
        assert!(
            !terrariumd::question_guard::check_text(&state(&td), &id, end, text)
                .await
                .unwrap()
        );
    }
    td.wait_status(&id, "idle").await;
    assert!(td.mail_of(lead["id"].as_str().unwrap()).await.is_empty());
}

#[tokio::test]
async fn registered_waits_and_tracked_questions_are_exempt() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    for (index, kind) in ["wait", "thread", "question", "bundle"]
        .into_iter()
        .enumerate()
    {
        let name = format!("w{index}");
        let (id, aid) = running(&td, "p", &name, json!({})).await;
        let end = turn(&td, &id, aid, async || {}).await;
        if kind == "wait" {
            td.post_as(
                &id,
                &format!("/api/agents/{id}/waits"),
                json!({"kind": "time", "for": "1h"}),
            )
            .await
            .ok();
        } else {
            let thread = td
                .post(
                    "/api/threads",
                    json!({"topic": "Decision", "scope": "p", "agent": format!("p/{name}")}),
                )
                .await
                .ok()["id"]
                .as_str()
                .unwrap()
                .to_owned();
            if kind == "thread" {
                let st = state(&td);
                st.db
                    .write("system", move |t| {
                        t.execute(
                            "UPDATE threads SET status = 'waiting_operator' WHERE id = ?1",
                            [thread],
                        )?;
                        Ok(())
                    })
                    .await
                    .unwrap();
            } else {
                let mut body = json!({"kind": kind, "text": "Choose a branch", "thread": thread});
                if kind == "bundle" {
                    body["items"] = json!([{"text": "Which branch?", "options": ["next", "main"]}]);
                }
                td.post_as(&id, &format!("/api/agents/{id}/questions"), body)
                    .await
                    .ok();
                // Isolate the open question exemption from the thread status.
                state(&td)
                    .db
                    .write("system", move |t| {
                        t.execute(
                            "UPDATE threads SET status = 'waiting_agent' WHERE id = ?1",
                            [thread],
                        )?;
                        Ok(())
                    })
                    .await
                    .unwrap();
            }
        }
        // Read the facts at a fresh turn end, so a newer turn or mail in
        // the setup cannot accidentally supply the exemption under test.
        td.wait_status(&id, if kind == "wait" { "waiting" } else { "idle" })
            .await;
        let end = turn(&td, &id, aid, async || {}).await.max(end);
        assert!(
            !terrariumd::question_guard::check_text(&state(&td), &id, end, "Should I continue?")
                .await
                .unwrap(),
            "{kind}"
        );
        assert!(
            td.get(&format!("/api/agents/{id}/events?kind=question_guard"))
                .await
                .ok()
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn a_worker_question_blocks_the_agent_and_mails_the_lead() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let lead = td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let (id, aid) = running(&td, "p", "w", json!({})).await;
    let end = turn(&td, &id, aid, async || {}).await;
    let st = state(&td);

    // A report is not a question.
    let done = terrariumd::question_guard::check_text(&st, &id, end, "Done. Tests pass.")
        .await
        .unwrap();
    assert!(!done);
    td.wait_status(&id, "idle").await;

    let text = "I read the module.\n\nShould I delete the old `v1` API, or keep it?";
    let done = terrariumd::question_guard::check_text(&st, &id, end, text)
        .await
        .unwrap();
    assert!(done);
    let a = td.get(&format!("/api/agents/{id}")).await.ok().clone();
    assert_eq!(a["status"], "blocked");
    let status = td
        .get(&format!("/api/agents/{id}/events?kind=status"))
        .await;
    let last = status.ok().as_array().unwrap().last().unwrap().clone();
    assert_eq!(
        last["data"]["reason"],
        "asked a question in plain text: Should I delete the old `v1` API, or keep it?"
    );
    // The lead has the question as quoted data, from `system`.
    let mail = td.mail_of(lead["id"].as_str().unwrap()).await;
    let m = mail
        .iter()
        .find(|m| m["subject"] == "p/w asked a question in plain text")
        .unwrap_or_else(|| panic!("{mail:#?}"));
    assert_eq!(m["from"], "system");
    let body = td
        .get_as(
            lead["id"].as_str().unwrap(),
            &format!("/api/mail/{}", m["id"].as_str().unwrap()),
        )
        .await
        .ok()["body"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        body.contains("> Should I delete the old `v1` API, or keep it?"),
        "{body}"
    );
    assert!(body.contains("terrarium mail send p/w"), "{body}");
    // The guard acts once: the agent is no longer resting.
    let again = terrariumd::question_guard::check_text(&st, &id, end, text)
        .await
        .unwrap();
    assert!(!again);
}

#[tokio::test]
async fn with_no_lead_the_operator_gets_the_mail() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let (id, aid) = running(&td, "p", "w", json!({})).await;
    let end = turn(&td, &id, aid, async || {}).await;
    let done = terrariumd::question_guard::check_text(
        &state(&td),
        &id,
        end,
        "Would you like me to also update the docs?",
    )
    .await
    .unwrap();
    assert!(done);
    let mail = td.get("/api/mail?to=operator").await;
    assert!(
        mail.ok()
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["subject"] == "p/w asked a question in plain text"),
        "{}",
        mail.body
    );
}

#[tokio::test]
async fn leads_and_turns_that_reported_are_exempt() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let q = "Should I go on?";

    // A lead talks to the operator: exempt.
    let (lid, laid) = running(&td, "p", "lead", json!({"roles": ["project_admin"]})).await;
    let end = turn(&td, &lid, laid, async || {}).await;
    let st = state(&td);
    assert!(
        !terrariumd::question_guard::check_text(&st, &lid, end, q)
            .await
            .unwrap()
    );

    // A worker that mailed in the turn.
    let (id, aid) = running(&td, "p", "w", json!({})).await;
    let end = turn(&td, &id, aid, async || {
        td.post_as(
            &id,
            "/api/mail",
            json!({"to": "operator", "subject": "q", "body": "Should I go on?"}),
        )
        .await
        .ok();
    })
    .await;
    assert!(
        !terrariumd::question_guard::check_text(&st, &id, end, q)
            .await
            .unwrap()
    );

    // A worker that set `blocked` itself.
    let (id2, aid2) = running(&td, "p", "w2", json!({})).await;
    let end = turn(&td, &id2, aid2, async || {
        td.post_as(
            &id2,
            &format!("/api/agents/{id2}/status"),
            json!({"status": "blocked", "note": "need a decision"}),
        )
        .await
        .ok();
    })
    .await;
    assert!(
        !terrariumd::question_guard::check_text(&st, &id2, end, q)
            .await
            .unwrap()
    );

    // A new turn started after the end: too late.
    let (id3, aid3) = running(&td, "p", "w3", json!({})).await;
    let end = turn(&td, &id3, aid3, async || {}).await;
    td.sup.emit_harness(aid3, Event::TurnStart { prompt: None });
    seq_of(&td, &id3, "turn_start", end).await;
    assert!(
        !terrariumd::question_guard::check_text(&st, &id3, end, q)
            .await
            .unwrap()
    );
    for a in [&lid, &id, &id2, &id3] {
        let v = td.get(&format!("/api/agents/{a}")).await.ok().clone();
        assert_ne!(v["status"], "blocked", "{a}");
    }
}
