//! Cards that an Inbox thread tracks (TER-146, TER-144 D2, docs/api.md
//! §6.13): the lead links a new card (`inbox_thread` on the card create)
//! and an existing one (`POST /api/threads/{id}/cards`), the attached
//! agent links cards as it tracks (`POST .../track` with `cards`), cards
//! of two projects, the refusals without the read rights, and the list on
//! the thread. A link gives no read access: a reader of the thread sees
//! only the linked cards whose project it reads.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::TestDaemon;
use serde_json::{Value, json};

struct Lab {
    td: TestDaemon,
    /// `p/lead`: the report-to lead of the `p` threads.
    lead: String,
    /// `p/arch`: the attached agent.
    arch: String,
    /// `p/w`: a member of `p` that is not in the thread.
    w: String,
    /// `meta/architect`: reads all projects and threads.
    architect: String,
    /// The `p` thread, attached to `p/arch`.
    tid: String,
}

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

async fn lab() -> Lab {
    let td = TestDaemon::start().await;
    for p in ["meta", "p", "q"] {
        td.project(p).await;
    }
    td.agent("meta", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let lead = id(&td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await);
    td.agent("q", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let arch = id(&td.agent("p", "arch", json!({})).await);
    let w = id(&td.agent("p", "w", json!({})).await);
    let architect = id(&td
        .agent("meta", "architect", json!({"roles": ["architect"]}))
        .await);
    let r = td
        .post(
            "/api/threads",
            json!({"topic": "Board columns", "scope": "p", "agent": "arch",
                   "text": "Five columns, please."}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let tid = id(&r.body);
    Lab {
        td,
        lead,
        arch,
        w,
        architect,
        tid,
    }
}

impl Lab {
    /// A card of `project`, made by the operator.
    async fn card(&self, project: &str, title: &str) -> String {
        let r = self
            .td
            .post(
                &format!("/api/projects/{project}/cards"),
                json!({"title": title}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
        id(&r.body)
    }

    async fn thread(&self) -> Value {
        self.td
            .get(&format!("/api/threads/{}", self.tid))
            .await
            .ok()
            .clone()
    }

    /// The linked cards of the thread as `project:card`.
    async fn cards(&self) -> Vec<String> {
        self.thread().await["cards"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| format!("{}:{}", c["project"].as_str().unwrap(), id_of(c)))
            .collect()
    }

    fn link_path(&self) -> String {
        format!("/api/threads/{}/cards", self.tid)
    }
}

fn id_of(c: &Value) -> &str {
    c["card"].as_str().unwrap()
}

#[tokio::test]
async fn the_lead_links_a_new_and_an_existing_card() {
    let lab = lab().await;
    // A new thread lists no cards.
    assert_eq!(lab.thread().await["cards"], json!([]));

    // The lead makes a card linked to the thread.
    let r = lab
        .td
        .post_as(
            &lab.lead,
            "/api/projects/p/cards",
            json!({"title": "Add the fifth column", "inbox_thread": lab.tid}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let c1 = id(&r.body);

    // The lead links an existing card.
    let c2 = lab.card("p", "Docs for five columns").await;
    let r = lab
        .td
        .post_as(&lab.lead, &lab.link_path(), json!({"card": c2}))
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["id"], lab.tid.as_str());
    assert_eq!(r.body["cards"].as_array().unwrap().len(), 2);

    let th = lab.thread().await;
    let cards = th["cards"].as_array().unwrap();
    assert_eq!(cards[0]["project"], "p");
    assert_eq!(cards[0]["card"], c1.as_str());
    assert_eq!(cards[0]["linked_by"], lab.lead.as_str());
    assert_eq!(cards[0]["linked_by_name"], "p/lead");
    assert!(cards[0]["linked_at"].is_string());
    assert_eq!(cards[1]["card"], c2.as_str());
    // A link changes no status and wakes nobody.
    assert_eq!(th["status"], "waiting_agent");

    // A second link of the same card keeps one row.
    let r = lab
        .td
        .post_as(
            &lab.lead,
            &lab.link_path(),
            json!({"card": format!("p:{c2}")}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(lab.cards().await, [format!("p:{c1}"), format!("p:{c2}")]);

    // The thread list has no `cards`: only the thread itself lists them.
    let list = lab.td.get("/api/threads").await.ok().clone();
    let row = list
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == lab.tid.as_str())
        .unwrap()
        .clone();
    assert!(row.get("cards").is_none(), "{row}");

    // The link is audited.
    let audit = lab
        .td
        .get("/api/audit?action=thread.card_link")
        .await
        .ok()
        .clone();
    assert_eq!(audit.as_array().unwrap().len(), 2, "{audit}");
}

#[tokio::test]
async fn the_agent_links_cards_of_two_projects_as_it_tracks() {
    let lab = lab().await;
    let pc = lab.card("p", "Fifth column").await;
    let qc = lab.card("q", "Fifth column on q").await;

    // The architect reads all: it links a card of `q` to the `p` thread.
    let r = lab
        .td
        .post_as(
            &lab.architect,
            &lab.link_path(),
            json!({"card": format!("q:{qc}")}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);

    // The attached agent links its own project's card and tracks.
    let r = lab
        .td
        .post_as(
            &lab.arch,
            &format!("/api/threads/{}/track", lab.tid),
            json!({"cards": [format!("p:{pc}")]}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["status"], "tracking");
    assert_eq!(lab.cards().await, [format!("q:{qc}"), format!("p:{pc}")]);

    // The operator links a card of any project too.
    let qc2 = lab.card("q", "Second q card").await;
    let r = lab.td.post(&lab.link_path(), json!({"card": qc2})).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["cards"][2]["project"], "q");
    assert_eq!(r.body["cards"][2]["linked_by"], "operator");
    assert_eq!(r.body["status"], "tracking");

    // The track with no body still works (TER-145).
    let r = lab
        .td
        .post_empty_as(&lab.arch, &format!("/api/threads/{}/track", lab.tid))
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
}

#[tokio::test]
async fn a_link_needs_the_read_rights_and_gives_none() {
    let lab = lab().await;
    let pc = lab.card("p", "Fifth column").await;
    let qc = lab.card("q", "Fifth column on q").await;

    // `p/w` does not read the thread: 404, from both sides, and the card
    // that it made is not kept.
    lab.td
        .post_as(&lab.w, &lab.link_path(), json!({"card": pc}))
        .await
        .err(404, "not_found");
    let before = lab.td.get("/api/projects/p/cards").await.ok().clone();
    lab.td
        .post_as(
            &lab.w,
            "/api/projects/p/cards",
            json!({"title": "Sneaky", "inbox_thread": lab.tid}),
        )
        .await
        .err(404, "not_found");
    let after = lab.td.get("/api/projects/p/cards").await.ok().clone();
    assert_eq!(before, after);

    // The lead of `p` reads the thread but not project `q`: the same 404
    // as for a card that does not exist, with or without the project, so
    // the refusal does not tell that the card exists or name its project.
    for card in [format!("q:{qc}"), qc.clone()] {
        let r = lab
            .td
            .post_as(&lab.lead, &lab.link_path(), json!({"card": card}))
            .await;
        r.err(404, "not_found");
        assert_eq!(
            r.body["error"]["message"],
            format!("no card `{qc}`"),
            "{}",
            r.body
        );
    }
    let r = lab
        .td
        .post_as(&lab.lead, &lab.link_path(), json!({"card": "q:Q-999"}))
        .await;
    r.err(404, "not_found");
    assert_eq!(r.body["error"]["message"], "no card `Q-999`", "{}", r.body);

    // The attached agent with a `q` card: 404, and nothing changes; the
    // thread does not track.
    lab.td
        .post_as(
            &lab.arch,
            &format!("/api/threads/{}/track", lab.tid),
            json!({"cards": [format!("p:{pc}"), format!("q:{qc}")]}),
        )
        .await
        .err(404, "not_found");
    let th = lab.thread().await;
    assert_eq!(th["cards"], json!([]));
    assert_eq!(th["status"], "waiting_agent");

    // A card that is not in the named project, an unknown card, a bad
    // reference.
    lab.td
        .post(&lab.link_path(), json!({"card": format!("q:{pc}")}))
        .await
        .err(404, "not_found");
    lab.td
        .post(&lab.link_path(), json!({"card": "p:NOPE-1"}))
        .await
        .err(404, "not_found");
    lab.td
        .post(&lab.link_path(), json!({"card": "p:"}))
        .await
        .err(422, "invalid");

    // The operator links the `q` card. The lead of `p` reads the thread
    // but not project `q`: the thread does not list the card, and the
    // link gives no read access to it.
    lab.td
        .post(&lab.link_path(), json!({"card": qc}))
        .await
        .ok();
    let th = lab
        .td
        .get_as(&lab.lead, &format!("/api/threads/{}", lab.tid))
        .await
        .ok()
        .clone();
    assert_eq!(th["cards"], json!([]), "{th}");
    assert!(!th.to_string().contains(qc.as_str()), "{th}");
    // The operator and the architect read `q`: they see it.
    assert_eq!(lab.cards().await, [format!("q:{qc}")]);
    let th = lab
        .td
        .get_as(&lab.architect, &format!("/api/threads/{}", lab.tid))
        .await
        .ok()
        .clone();
    assert_eq!(th["cards"][0]["card"], qc.as_str());
    // The answers of the writes filter the same way: the lead links a `p`
    // card and the agent tracks, and each sees only the `p` card.
    let r = lab
        .td
        .post_as(&lab.lead, &lab.link_path(), json!({"card": pc}))
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["cards"].as_array().unwrap().len(), 1, "{}", r.body);
    assert_eq!(r.body["cards"][0]["card"], pc.as_str());
    let r = lab
        .td
        .post_empty_as(&lab.arch, &format!("/api/threads/{}/track", lab.tid))
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["cards"].as_array().unwrap().len(), 1, "{}", r.body);
    assert_eq!(r.body["cards"][0]["card"], pc.as_str());
    assert_eq!(lab.cards().await, [format!("q:{qc}"), format!("p:{pc}")]);
    lab.td
        .get_as(&lab.lead, &format!("/api/cards/{qc}"))
        .await
        .err(403, "forbidden");
    // Nor to the thread: `p/w` still cannot read it.
    lab.td
        .get_as(&lab.w, &format!("/api/threads/{}", lab.tid))
        .await
        .err(404, "not_found");

    // A closed thread takes no link.
    lab.td
        .post(&format!("/api/threads/{}/close", lab.tid), json!({}))
        .await
        .ok();
    lab.td
        .post(&lab.link_path(), json!({"card": pc}))
        .await
        .err(409, "conflict");
}

#[tokio::test]
async fn an_archived_card_is_not_linked() {
    let lab = lab().await;
    let pc = lab.card("p", "Old column").await;
    let r = lab.td.delete(&format!("/api/cards/{pc}")).await;
    assert_eq!(r.status, 204, "{}", r.body);
    lab.td
        .post_as(&lab.lead, &lab.link_path(), json!({"card": pc}))
        .await
        .err(422, "invalid");
    lab.td
        .post_as(
            &lab.arch,
            &format!("/api/threads/{}/track", lab.tid),
            json!({"cards": [format!("p:{pc}")]}),
        )
        .await
        .err(422, "invalid");
    let th = lab.thread().await;
    assert_eq!(th["cards"], json!([]));
    assert_eq!(th["status"], "waiting_agent");
}
