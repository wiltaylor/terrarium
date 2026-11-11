//! Inbox threads (TER-91, docs/api.md §6.13): the thread item and its API,
//! the report-to rule (D34), and questions linked to a thread; the right
//! `thread:open` and the rights check of agent-opened threads (TER-93, A1).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::TestDaemon;
use serde_json::{Value, json};

struct Lab {
    td: TestDaemon,
    /// `meta/lead`: the lead of the global threads.
    meta_lead: String,
    /// `p/lead`: the lead of project `p`.
    p_lead: String,
    /// `p/arch`: a member of `p`, the attached agent.
    arch: String,
    /// `p/w`: a member of `p` with no thread.
    w: String,
}

async fn lab() -> Lab {
    let td = TestDaemon::start().await;
    td.project("meta").await;
    td.project("p").await;
    td.project("q").await;
    let id = |v: Value| v["id"].as_str().unwrap().to_owned();
    let meta_lead = id(td
        .agent("meta", "lead", json!({"roles": ["project_admin"]}))
        .await);
    let p_lead = id(td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await);
    // Another project_admin of `p`: the one named `lead` is the lead.
    td.agent("p", "admin2", json!({"roles": ["project_admin"]}))
        .await;
    let arch = id(td.agent("p", "arch", json!({})).await);
    let w = id(td.agent("p", "w", json!({})).await);
    Lab {
        td,
        meta_lead,
        p_lead,
        arch,
        w,
    }
}

impl Lab {
    async fn open(&self, body: Value) -> Value {
        let r = self.td.post("/api/threads", body).await;
        assert_eq!(r.status, 201, "{}", r.body);
        r.body
    }

    async fn thread(&self, id: &str) -> Value {
        self.td
            .get(&format!("/api/threads/{id}"))
            .await
            .ok()
            .clone()
    }
}

#[tokio::test]
async fn multiple_open_questions_and_bundles_per_thread_and_notice_replies() {
    let lab = lab().await;
    let t = lab.open(json!({"topic": "Decision", "scope": "p", "agent": "lead", "text": "Please investigate"})).await;
    let path = format!("/api/agents/{}/questions", lab.p_lead);
    let tid = t["id"].as_str().unwrap();
    let q = lab
        .td
        .post_as(
            &lab.p_lead,
            &path,
            json!({"text": "Choose A?", "thread": tid}),
        )
        .await;
    assert_eq!(q.status, 201, "{}", q.body);
    for body in [
        json!({"text": "Choose B?", "thread": tid}),
        json!({"kind": "bundle", "text": "Choices", "thread": tid,
            "items": [{"text": "C?"}, {"text": "D?"}]}),
        json!({"kind": "bundle", "text": "More choices", "thread": tid,
            "items": [{"text": "G?"}, {"text": "H?"}]}),
    ] {
        let r = lab.td.post_as(&lab.p_lead, &path, body).await;
        assert_eq!(r.status, 201, "{}", r.body);
    }
    let r = lab.td.post_as(&lab.p_lead, &path, json!({"kind": "notice", "text": "Answer to your request", "reply_to": t["messages"][0]["id"]})).await;
    r.err(422, "invalid");
    let r = lab
        .td
        .post_as(
            &lab.p_lead,
            &format!("/api/threads/{tid}/messages"),
            json!({"text": "Answer to your request"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert!(
        r.body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["text"] == "Answer to your request")
    );
    let other = lab
        .open(json!({"topic": "Other decision", "scope": "p", "agent": "lead"}))
        .await;
    let r = lab
        .td
        .post_as(
            &lab.p_lead,
            &path,
            json!({"kind": "bundle", "text": "Other choices", "thread": other["id"],
        "items": [{"text": "E?"}, {"text": "F?"}]}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let another = lab
        .td
        .post_as(
            &lab.p_lead,
            &path,
            json!({"text": "Another?", "thread": other["id"]}),
        )
        .await;
    assert_eq!(another.status, 201, "{}", another.body);
    lab.td
        .post(
            &format!("/api/questions/{}/answer", q.body["id"].as_str().unwrap()),
            json!({"answer": "Yes"}),
        )
        .await
        .ok();
    assert_eq!(lab.thread(tid).await["status"], "waiting_operator");
}

#[tokio::test]
async fn a_thread_opens_talks_responds_and_closes() {
    let lab = lab().await;
    // A global thread reports to the lead of `meta`; with a first message
    // of the operator it waits for the agent.
    let g = lab
        .open(
            json!({"topic": "Shard the event store?", "template": "architect",
                     "agent": "p/arch", "text": "How would you shard it?"}),
        )
        .await;
    assert_eq!(g["scope"], "global");
    assert_eq!(g["template"], "architect");
    assert_eq!(g["opened_by"], "operator");
    assert_eq!(g["agent"], lab.arch.as_str());
    assert_eq!(g["agent_name"], "p/arch");
    assert_eq!(g["report_to"], lab.meta_lead.as_str());
    assert_eq!(g["report_to_name"], "meta/lead");
    assert_eq!(g["status"], "waiting_agent");
    assert_eq!(g["messages"].as_array().unwrap().len(), 1);
    assert_eq!(g["messages"][0]["author"], "operator");
    assert_eq!(g["messages"][0]["kind"], "message");
    let gid = g["id"].as_str().unwrap().to_owned();

    // A project thread reports to the project's lead (`lead`, not
    // `admin2`); with no first message it waits for the operator.
    let t = lab
        .open(json!({"topic": "Board columns", "scope": "p", "agent": "arch"}))
        .await;
    assert_eq!(t["scope"], "p");
    assert_eq!(t["report_to"], lab.p_lead.as_str());
    assert_eq!(t["status"], "waiting_operator");
    let tid = t["id"].as_str().unwrap().to_owned();

    // The agent posts: the thread waits for the operator. The operator
    // posts: it waits for the agent.
    let r = lab
        .td
        .post_as(
            &lab.arch,
            &format!("/api/threads/{tid}/messages"),
            json!({"text": "Four columns, **here** is why."}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["status"], "waiting_operator");
    let r = lab
        .td
        .post(
            &format!("/api/threads/{tid}/messages"),
            json!({"text": "And blocked?"}),
        )
        .await;
    assert_eq!(r.body["status"], "waiting_agent");
    // The overall response: the approval record, and a message.
    let r = lab
        .td
        .post(
            &format!("/api/threads/{tid}/respond"),
            json!({"text": "Approved: four columns plus blocked."}),
        )
        .await;
    let th = r.ok().clone();
    assert_eq!(
        th["response"]["text"],
        "Approved: four columns plus blocked."
    );
    assert_eq!(th["response"]["by"], "operator");
    assert_eq!(th["status"], "waiting_agent");
    let msgs = th["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[2]["kind"], "response");
    assert_eq!(msgs[0]["author_name"], "p/arch");

    // The list: the operator sees both, the one that waits for it first
    // (none waits now), with counts and no messages.
    let all = lab.td.get("/api/threads").await.ok().clone();
    let all = all.as_array().unwrap();
    assert_eq!(all.len(), 2);
    assert!(all.iter().all(|t| t.get("messages").is_none()));
    let item = all.iter().find(|t| t["id"] == tid.as_str()).unwrap();
    assert_eq!(item["message_count"], 3);
    assert_eq!(item["open_questions"], 0);
    // The attached agent sees both; the report-to lead of `p` sees only the
    // project thread; another agent sees none and gets 404 for one.
    let mine = lab.td.get_as(&lab.arch, "/api/threads").await.ok().clone();
    assert_eq!(mine.as_array().unwrap().len(), 2);
    let lead = lab
        .td
        .get_as(&lab.p_lead, "/api/threads")
        .await
        .ok()
        .clone();
    let lead = lead.as_array().unwrap();
    assert_eq!(lead.len(), 1);
    assert_eq!(lead[0]["id"], tid.as_str());
    let other = lab.td.get_as(&lab.w, "/api/threads").await.ok().clone();
    assert_eq!(other.as_array().unwrap().len(), 0);
    lab.td
        .get_as(&lab.w, &format!("/api/threads/{tid}"))
        .await
        .err(404, "not_found");
    lab.td
        .get_as(&lab.meta_lead, &format!("/api/threads/{gid}"))
        .await
        .ok();

    // Rights: an agent without `thread:open` does not open, the report-to
    // lead reads but does not post, only the operator responds.
    lab.td
        .post_as(
            &lab.p_lead,
            "/api/threads",
            json!({"topic": "x", "scope": "p"}),
        )
        .await
        .err(403, "forbidden");
    lab.td
        .post_as(
            &lab.p_lead,
            &format!("/api/threads/{tid}/messages"),
            json!({"text": "hi"}),
        )
        .await
        .err(403, "forbidden");
    lab.td
        .post_as(
            &lab.arch,
            &format!("/api/threads/{tid}/respond"),
            json!({"text": "yes"}),
        )
        .await
        .err(403, "forbidden");

    // The agent closes with a handoff; a body that does not parse and a
    // bad link kind are refused first.
    lab.td
        .post_as(
            &lab.arch,
            &format!("/api/threads/{tid}/close"),
            json!({"bogus": 1}),
        )
        .await
        .err(400, "bad_request");
    lab.td
        .post_as(
            &lab.arch,
            &format!("/api/threads/{tid}/close"),
            json!({"summary": "done", "links": [{"kind": "ftp", "ref": "x"}]}),
        )
        .await
        .err(422, "invalid");
    let r = lab
        .td
        .post_as(
            &lab.arch,
            &format!("/api/threads/{tid}/close"),
            json!({"summary": "Cards cut for the board columns.",
                   "links": [{"kind": "card", "ref": "P-1"}, {"kind": "wiki", "ref": "board"}]}),
        )
        .await;
    // The agent's close is a proposal (TER-95); the operator confirms it.
    let th = r.ok().clone();
    assert_eq!(th["status"], "waiting_operator");
    assert_eq!(
        th["close_proposal"]["summary"],
        "Cards cut for the board columns."
    );
    let th = lab
        .td
        .post(
            &format!("/api/threads/{tid}/close"),
            json!({"confirm": true}),
        )
        .await
        .ok()
        .clone();
    assert_eq!(th["status"], "closed");
    assert_eq!(th["closed_by"], "operator");
    assert_eq!(th["close_reason"], "confirmed");
    assert_eq!(th["handoff"]["by"], lab.arch.as_str());
    assert_eq!(th["handoff"]["summary"], "Cards cut for the board columns.");
    assert_eq!(th["handoff"]["links"][0]["kind"], "card");
    assert_eq!(th["handoff"]["links"][0]["ref"], "P-1");
    // A closed thread does not change.
    lab.td
        .post(
            &format!("/api/threads/{tid}/messages"),
            json!({"text": "more"}),
        )
        .await
        .err(409, "conflict");
    lab.td
        .post_empty(&format!("/api/threads/{tid}/close"))
        .await
        .err(409, "conflict");
    // The default list hides it; `closed` and `all` show it.
    let open = lab.td.get("/api/threads").await.ok().clone();
    assert_eq!(open.as_array().unwrap().len(), 1);
    let closed = lab.td.get("/api/threads?status=closed").await.ok().clone();
    assert_eq!(closed.as_array().unwrap()[0]["id"], tid.as_str());
    let every = lab.td.get("/api/threads?status=all").await.ok().clone();
    assert_eq!(every.as_array().unwrap().len(), 2);
    // The operator closes the other one with no body.
    let th = lab
        .td
        .post_empty(&format!("/api/threads/{gid}/close"))
        .await
        .ok()
        .clone();
    assert_eq!(th["closed_by"], "operator");
    assert_eq!(th["handoff"]["summary"], "");

    // The audit keeps open, respond, and close.
    for action in ["thread.open", "thread.respond", "thread.close"] {
        let a = lab
            .td
            .get(&format!("/api/audit?action={action}"))
            .await
            .ok()
            .clone();
        assert!(!a.as_array().unwrap().is_empty(), "{action}");
    }
}

#[tokio::test]
async fn the_open_checks_its_fields_and_the_lead() {
    let lab = lab().await;
    lab.td
        .post("/api/threads", json!({"topic": "  "}))
        .await
        .err(422, "invalid");
    lab.td
        .post("/api/threads", json!({"topic": "two\nlines"}))
        .await
        .err(422, "invalid");
    lab.td
        .post("/api/threads", json!({"topic": "x", "scope": "nope"}))
        .await
        .err(404, "not_found");
    // Project `q` has no project_admin: nobody to report to (D34).
    lab.td
        .post("/api/threads", json!({"topic": "x", "scope": "q"}))
        .await
        .err(409, "conflict");
    // A project thread's agent is of its project.
    lab.td
        .agent("q", "qa", json!({"roles": ["project_admin"]}))
        .await;
    lab.td
        .post(
            "/api/threads",
            json!({"topic": "x", "scope": "q", "agent": "p/arch"}),
        )
        .await
        .err(422, "invalid");
    let r = lab
        .td
        .post("/api/threads", json!({"topic": "x", "bogus": 1}))
        .await;
    assert!(r.status.is_client_error(), "{}", r.body);
    lab.td
        .get("/api/threads?status=nope")
        .await
        .err(422, "invalid");
}

#[tokio::test]
async fn questions_in_a_thread_show_in_it_and_drive_its_status() {
    let lab = lab().await;
    let t = lab
        .open(
            json!({"topic": "Retention", "scope": "p", "agent": "p/arch",
                     "text": "What retention do we need?"}),
        )
        .await;
    let tid = t["id"].as_str().unwrap().to_owned();
    let ask = |body: Value| {
        let td = &lab.td;
        let arch = lab.arch.clone();
        async move {
            td.post_as(&arch, &format!("/api/agents/{arch}/questions"), body)
                .await
        }
    };
    // `p/arch` is no leader: it may not ask the operator on its own ...
    ask(json!({"text": "Keep 30 days?", "options": ["yes", "no"]}))
        .await
        .err(403, "forbidden");
    // ... but it may ask in its own thread.
    let r = ask(json!({"text": "Keep 30 days?", "options": ["yes", "no"],
                       "priority": "urgent", "thread": tid}))
    .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["thread"], tid.as_str());
    let qid = r.body["id"].as_str().unwrap().to_owned();
    // Its thread remains sufficient context when the operator clears card.
    lab.td
        .patch(&format!("/api/questions/{qid}"), json!({"card": null}))
        .await
        .ok();
    // An independent block can stay open while the first is answered.
    let another = ask(json!({"text": "Another?", "thread": tid})).await;
    assert_eq!(another.status, 201, "{}", another.body);
    lab.td
        .post(
            &format!("/api/questions/{qid}/answer"),
            json!({"option": "yes", "answer": "Keep it for compliance."}),
        )
        .await
        .ok();
    let th = lab.thread(&tid).await;
    assert_eq!(th["status"], "waiting_operator");
    assert_eq!(th["questions"][0]["answer_option"], 0);
    assert_eq!(th["questions"][0]["answer"], "Keep it for compliance.");
    lab.td
        .post(
            &format!(
                "/api/questions/{}/answer",
                another.body["id"].as_str().unwrap()
            ),
            json!({"answer": "Yes"}),
        )
        .await
        .ok();
    assert_eq!(lab.thread(&tid).await["status"], "waiting_agent");
    // A bundle can follow the answered question.
    let r = ask(json!({"kind": "bundle", "text": "Two more", "thread": tid,
                       "priority": "urgent",
                       "items": [{"text": "Backups?", "options": ["daily", "weekly"]},
                                 {"text": "Region?", "options": ["eu", "us"]}]}))
    .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let bid = r.body["id"].as_str().unwrap().to_owned();
    let th = lab.thread(&tid).await;
    assert_eq!(th["status"], "waiting_operator");
    let qs = th["questions"].as_array().unwrap();
    assert_eq!(qs.len(), 3);
    assert_eq!(qs[0]["id"], qid.as_str());
    assert_eq!(qs[2]["kind"], "bundle");

    // Not in a thread of another agent, nor in an unknown thread.
    let other = lab
        .open(json!({"topic": "Other", "scope": "p", "agent": "p/w"}))
        .await;
    let oid = other["id"].as_str().unwrap();
    ask(json!({"text": "Mine?", "thread": oid}))
        .await
        .err(409, "conflict");
    ask(json!({"text": "Where?", "thread": "01NOPE"}))
        .await
        .err(404, "not_found");

    // The Inbox: one thread item, open and urgent (by its urgent question);
    // the questions are not items of their own.
    let items = lab.td.get("/api/inbox?kind=thread").await.ok().clone();
    let item = items
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == format!("inbox-thread:{tid}"))
        .unwrap()
        .clone();
    assert_eq!(item["kind"], "thread");
    assert_eq!(item["open"], true);
    assert_eq!(item["urgent"], true);
    assert_eq!(item["thread"]["open_questions"], 1);
    let qitems = lab.td.get("/api/inbox?kind=question").await.ok().clone();
    assert!(
        !qitems
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["question"]["thread"] == tid.as_str()),
        "{qitems}"
    );
    let counts = lab.td.get("/api/inbox/counts").await.ok().clone();
    // The two waiting threads (`tid` and `other`, which has no first
    // message); no question counts on its own.
    assert_eq!(counts["thread"], 2, "{counts}");
    assert_eq!(counts["question"], 0, "{counts}");
    // The threads have their own badge (TER-466): the Inbox totals leave
    // them out, and `thread_urgent` counts the urgent one.
    let kinds: i64 = ["question", "notice", "mail", "system"]
        .iter()
        .map(|k| counts[*k].as_i64().unwrap())
        .sum();
    assert_eq!(counts["open"].as_i64().unwrap(), kinds, "{counts}");
    assert_eq!(counts["urgent"], 0, "{counts}");
    assert_eq!(counts["critical"], 0, "{counts}");
    assert_eq!(counts["thread_urgent"], 1, "{counts}");
    assert_eq!(counts["thread_critical"], 0, "{counts}");
    // Nor do the open, All, and default lists show them; `kind=thread` does.
    for q in ["", "?status=open", "?status=all"] {
        let r = lab.td.get(&format!("/api/inbox{q}")).await;
        assert_eq!(r.headers["x-inbox-open"], kinds.to_string(), "{q}");
        let rows = r.ok().as_array().unwrap().clone();
        assert!(!rows.iter().any(|i| i["kind"] == "thread"), "{q}: {rows:?}");
    }

    // Answering the bundle makes the thread wait for the agent.
    lab.td
        .post(
            &format!("/api/questions/{bid}/answer"),
            json!({"items": [{"option": "daily", "answer": "Every night."},
                             {"option": "eu", "answer": "For residency."}]}),
        )
        .await
        .ok();
    let th = lab.thread(&tid).await;
    assert_eq!(th["status"], "waiting_agent");
    let bundle = th["questions"].as_array().unwrap().iter()
        .find(|q| q["id"] == bid).unwrap();
    assert_eq!(bundle["items"][0]["answer_option"], 0);
    assert_eq!(bundle["items"][0]["answer"], "Every night.");
    assert_eq!(bundle["items"][1]["answer_option"], 0);
    assert_eq!(bundle["items"][1]["answer"], "For residency.");
    assert_eq!(th["questions"][1]["status"], "answered");
    let items = lab.td.get("/api/inbox?kind=thread").await.ok().clone();
    assert!(
        !items
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["id"] == format!("inbox-thread:{tid}")),
        "an item that waits for the agent is not open: {items}"
    );

    // The attached agent is not a leader, but can retract its own question.
    let own = ask(json!({"text": "No longer needed?", "thread": tid})).await;
    let own_id = own.ok()["id"].as_str().unwrap().to_owned();
    lab.td
        .post_as(
            &lab.arch,
            &format!("/api/questions/{own_id}/retract"),
            json!({"reason": "Resolved while investigating"}),
        )
        .await
        .ok();
    assert_eq!(
        lab.thread(&tid).await["questions"][3]["status"],
        "retracted"
    );

    // No question in a closed thread.
    lab.td
        .post_empty(&format!("/api/threads/{tid}/close"))
        .await
        .ok();
    ask(json!({"text": "One more?", "thread": tid}))
        .await
        .err(409, "conflict");
}

#[tokio::test]
async fn an_open_question_keeps_the_item_open_and_a_close_retracts_it() {
    let lab = lab().await;
    let t = lab
        .open(json!({"topic": "Retention", "scope": "p", "agent": "p/arch"}))
        .await;
    let tid = t["id"].as_str().unwrap().to_owned();
    let asks = format!("/api/agents/{}/questions", lab.arch);
    let r = lab
        .td
        .post_as(
            &lab.arch,
            &asks,
            json!({"text": "Keep 30 days?", "options": ["yes", "no"],
                   "priority": "urgent", "thread": tid}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let qid = r.body["id"].as_str().unwrap().to_owned();
    let item = |items: &Value| {
        items
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == format!("inbox-thread:{tid}"))
            .cloned()
    };

    // A message and a response of the operator make the thread wait for
    // the agent, but the open question keeps the item open, urgent and
    // counted.
    lab.td
        .post(
            &format!("/api/threads/{tid}/messages"),
            json!({"text": "Think about legal holds first."}),
        )
        .await
        .ok();
    lab.td
        .post(
            &format!("/api/threads/{tid}/respond"),
            json!({"text": "Go ahead with the plan."}),
        )
        .await
        .ok();
    assert_eq!(lab.thread(&tid).await["status"], "waiting_agent");
    let items = lab.td.get("/api/inbox?kind=thread").await.ok().clone();
    let it = item(&items).expect("the item with an open question is open");
    assert_eq!(it["open"], true);
    assert_eq!(it["urgent"], true);
    let counts = lab.td.get("/api/inbox/counts").await.ok().clone();
    assert_eq!(counts["thread"], 1, "{counts}");
    assert_eq!(counts["thread_urgent"], 1, "{counts}");
    assert_eq!(counts["urgent"], 0, "{counts}");

    // The operator's close retracts the open question: nothing stays open
    // and hidden in a closed thread.
    let th = lab
        .td
        .post_empty(&format!("/api/threads/{tid}/close"))
        .await
        .ok()
        .clone();
    assert_eq!(th["status"], "closed");
    assert_eq!(th["questions"][0]["status"], "retracted");
    assert_eq!(th["questions"][0]["retracted_by"], "operator");
    assert_eq!(
        th["questions"][0]["retract_reason"],
        format!("thread {tid} was closed")
    );
    let q = lab
        .td
        .get(&format!("/api/questions/{qid}"))
        .await
        .ok()
        .clone();
    assert_eq!(q["status"], "retracted");
    let items = lab.td.get("/api/inbox?kind=thread").await.ok().clone();
    assert!(item(&items).is_none(), "{items}");
    let counts = lab.td.get("/api/inbox/counts").await.ok().clone();
    assert_eq!(counts["thread"], 0, "{counts}");
    assert_eq!(counts["question"], 0, "{counts}");

    // The agent's close proposal keeps its open questions; the confirm
    // retracts them (TER-95).
    let t2 = lab
        .open(json!({"topic": "Backups", "scope": "p", "agent": "p/arch"}))
        .await;
    let t2 = t2["id"].as_str().unwrap().to_owned();
    lab.td
        .post_as(
            &lab.arch,
            &asks,
            json!({"text": "Daily?", "options": ["yes", "no"], "thread": t2}),
        )
        .await
        .ok();
    let th = lab
        .td
        .post_as(&lab.arch, &format!("/api/threads/{t2}/close"), json!({}))
        .await
        .ok()
        .clone();
    assert_eq!(th["questions"][0]["status"], "open");
    let th = lab
        .td
        .post(
            &format!("/api/threads/{t2}/close"),
            json!({"confirm": true}),
        )
        .await
        .ok()
        .clone();
    assert_eq!(th["questions"][0]["status"], "retracted");
    assert_eq!(th["questions"][0]["retracted_by"], "operator");
}

#[tokio::test]
async fn an_agent_that_reads_all_reads_and_lists_every_thread() {
    let lab = lab().await;
    let archi = lab
        .td
        .agent("q", "archi", json!({"roles": ["architect"]}))
        .await;
    let archi = archi["id"].as_str().unwrap().to_owned();
    let t = lab
        .open(json!({"topic": "Board columns", "scope": "p", "agent": "p/arch"}))
        .await;
    let tid = t["id"].as_str().unwrap().to_owned();
    lab.open(json!({"topic": "Global one", "agent": "p/w"}))
        .await;
    // An architect of another project lists and reads all threads ...
    let all = lab.td.get_as(&archi, "/api/threads").await.ok().clone();
    assert_eq!(all.as_array().unwrap().len(), 2, "{all}");
    let th = lab
        .td
        .get_as(&archi, &format!("/api/threads/{tid}"))
        .await
        .ok()
        .clone();
    assert_eq!(th["id"], tid.as_str());
    // ... but does not post or close.
    lab.td
        .post_as(
            &archi,
            &format!("/api/threads/{tid}/messages"),
            json!({"text": "hi"}),
        )
        .await
        .err(403, "forbidden");
    lab.td
        .post_as(&archi, &format!("/api/threads/{tid}/close"), json!({}))
        .await
        .err(403, "forbidden");
}

/// The templates of the rights tests: `consult` (project `p`, a member),
/// `p-admin` (project `p`, project_admin), and the global `helper` (a
/// member), `admin` (project_admin), and `architect` (architect).
async fn templates(td: &common::TestDaemon) {
    for (name, project, roles) in [
        ("consult", Some("p"), json!([])),
        ("p-admin", Some("p"), json!(["project_admin"])),
        ("helper", None, json!([])),
        ("admin", None, json!(["project_admin"])),
        ("architect", None, json!(["architect"])),
    ] {
        let mut body = json!({"name": name, "spec": {"harness": "claude", "roles": roles}});
        if let Some(p) = project {
            body["project"] = json!(p);
        }
        td.post("/api/templates", body).await.ok();
    }
}

/// Give `agent` the right `thread:open`, as `by` (`None`: the operator).
async fn grant(td: &common::TestDaemon, by: Option<&str>, agent: &str) -> common::Resp {
    let path = format!("/api/agents/{agent}");
    let body = json!({"policy": {"thread_open": true}});
    match by {
        Some(b) => td.patch_as(b, &path, body).await,
        None => td.patch(&path, body).await,
    }
}

#[tokio::test]
async fn an_agent_with_thread_open_opens_within_its_rights() {
    let lab = lab().await;
    templates(&lab.td).await;
    let open_as = |who: &str, body: Value| {
        let td = &lab.td;
        let who = who.to_owned();
        async move { td.post_as(&who, "/api/threads", body).await }
    };
    // No right: 403, with the way to get it.
    let r = open_as(&lab.w, json!({"topic": "x", "template": "consult"})).await;
    r.err(403, "forbidden");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("thread:open"),
        "{}",
        r.body
    );
    // The lead grants it to a member, not to itself; the operator grants
    // it to the lead.
    let g = grant(&lab.td, Some(&lab.p_lead), &lab.w).await;
    assert_eq!(g.ok()["policy"]["thread_open"], true);
    grant(&lab.td, Some(&lab.p_lead), &lab.p_lead)
        .await
        .err(403, "forbidden");
    grant(&lab.td, Some(&lab.w), &lab.arch)
        .await
        .err(403, "forbidden");
    grant(&lab.td, None, &lab.p_lead).await.ok();

    // The member opens with a member template: the scope is its project,
    // it reports to the project's lead, and the opener reads it.
    let r = open_as(
        &lab.w,
        json!({"topic": "Board columns", "template": "consult", "text": "Four?"}),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["scope"], "p");
    assert_eq!(r.body["opened_by"], lab.w.as_str());
    assert_eq!(r.body["opened_by_name"], "p/w");
    assert_eq!(r.body["report_to"], lab.p_lead.as_str());
    assert_eq!(r.body["status"], "waiting_operator");
    assert_eq!(r.body["messages"][0]["author"], lab.w.as_str());
    let tid = r.body["id"].as_str().unwrap().to_owned();
    lab.td
        .get_as(&lab.w, &format!("/api/threads/{tid}"))
        .await
        .ok();
    let mine = lab.td.get_as(&lab.w, "/api/threads").await.ok().clone();
    assert_eq!(mine.as_array().unwrap().len(), 1);
    // A global thread with a global member template is fine too (D35: no
    // limits on agent-opened threads).
    let r = open_as(
        &lab.w,
        json!({"topic": "Global", "scope": "global", "template": "helper"}),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["report_to"], lab.meta_lead.as_str());

    // An agent names a template; an unknown one is 404.
    let r = open_as(&lab.w, json!({"topic": "x"})).await;
    r.err(422, "invalid");
    let msg = r.body["error"]["message"].as_str().unwrap();
    assert!(!msg.contains("  "), "one space between words: {msg}");
    open_as(&lab.w, json!({"topic": "x", "template": "nope"}))
        .await
        .err(404, "not_found");
    // A1: a template above the opener's rights is refused, and the
    // message names the right.
    let r = open_as(&lab.w, json!({"topic": "x", "template": "p-admin"})).await;
    r.err(403, "forbidden");
    let msg = r.body["error"]["message"].as_str().unwrap();
    assert!(msg.contains("project_admin of `p`"), "{msg}");
    assert!(msg.contains("A1"), "{msg}");
    assert!(!msg.contains("  "), "one space between words: {msg}");
    open_as(
        &lab.w,
        json!({"topic": "x", "scope": "global", "template": "architect"}),
    )
    .await
    .err(403, "forbidden");
    // Another project's scope is a write outside the own project.
    open_as(
        &lab.w,
        json!({"topic": "x", "scope": "q", "template": "helper"}),
    )
    .await
    .err(403, "forbidden");
    // A named agent may not exceed the opener's rights either.
    let r = open_as(
        &lab.w,
        json!({"topic": "x", "template": "consult", "agent": "p/admin2"}),
    )
    .await;
    r.err(403, "forbidden");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("agent p/admin2"),
        "{}",
        r.body
    );
    let r = open_as(
        &lab.w,
        json!({"topic": "Mine", "template": "consult", "agent": "p/arch"}),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.body);

    // The lead of `p` uses the admin template of its project ...
    let r = open_as(&lab.p_lead, json!({"topic": "Lead", "template": "p-admin"})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    // ... but not a global admin template: that is project_admin of
    // `meta` (the reason for A1), and not the architect template.
    let r = open_as(
        &lab.p_lead,
        json!({"topic": "x", "scope": "global", "template": "admin"}),
    )
    .await;
    r.err(403, "forbidden");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("project_admin of `meta`"),
        "{}",
        r.body
    );
    open_as(
        &lab.p_lead,
        json!({"topic": "x", "scope": "global", "template": "architect"}),
    )
    .await
    .err(403, "forbidden");
    // The operator opens any thread with any template.
    lab.open(json!({"topic": "Op", "template": "architect"}))
        .await;
}

/// The architect role (TER-47) gets the thread rights (TER-96 decision):
/// it posts and asks in its thread as a member writes in its own project,
/// the rights check covers its opens, and it still writes nowhere else.
#[tokio::test]
async fn an_architect_posts_in_its_thread_and_writes_nowhere_else() {
    let lab = lab().await;
    templates(&lab.td).await;
    let arch = lab
        .td
        .agent("meta", "architect", json!({"roles": ["architect"]}))
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let g = lab
        .open(json!({"topic": "Shard it?", "agent": "meta/architect", "text": "How?"}))
        .await;
    let gid = g["id"].as_str().unwrap().to_owned();
    // It posts and asks in its own thread.
    let r = lab
        .td
        .post_as(
            &arch,
            &format!("/api/threads/{gid}/messages"),
            json!({"text": "By project."}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["status"], "waiting_operator");
    let r = lab
        .td
        .post_as(
            &arch,
            &format!("/api/agents/{arch}/questions"),
            json!({"text": "Keep the old store?", "options": ["yes", "no"], "thread": gid}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["thread"], gid.as_str());

    // It reads every thread (reads_all), but a thread of `p` is not its
    // own: it reads it and does not post to it.
    let t = lab
        .open(json!({"topic": "Columns", "scope": "p", "agent": "p/arch"}))
        .await;
    let tid = t["id"].as_str().unwrap();
    lab.td
        .post_as(
            &arch,
            &format!("/api/threads/{tid}/messages"),
            json!({"text": "Mine now."}),
        )
        .await
        .err(403, "forbidden");
    lab.td
        .get_as(&arch, &format!("/api/threads/{tid}"))
        .await
        .ok();
    // It still writes nothing in another project: no card in `p`.
    lab.td
        .post_as(
            &arch,
            "/api/projects/p/cards",
            json!({"title": "From the architect"}),
        )
        .await
        .err(403, "forbidden");

    // With thread:open it opens in its own project and global scope, with
    // a template within its rights, and not in another project.
    grant(&lab.td, None, &arch).await.ok();
    let r = lab
        .td
        .post_as(
            &arch,
            "/api/threads",
            json!({"topic": "Next", "scope": "global", "template": "architect"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    lab.td
        .post_as(
            &arch,
            "/api/threads",
            json!({"topic": "x", "scope": "global", "template": "admin"}),
        )
        .await
        .err(403, "forbidden");
    lab.td
        .post_as(
            &arch,
            "/api/threads",
            json!({"topic": "x", "scope": "p", "template": "consult"}),
        )
        .await
        .err(403, "forbidden");
}
