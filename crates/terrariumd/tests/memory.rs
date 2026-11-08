//! The memory graph and its rules (PLAN §14).
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

async fn mem(td: &TestDaemon, agent: &str, body: Value) -> Value {
    td.post_as(agent, "/api/projects/proj/memories", body)
        .await
        .ok()
        .clone()
}

fn ids(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn create_search_and_duplicates() {
    let td = setup().await;
    let m1 = mem(
        &td,
        "proj/w1",
        json!({"title": "The parser uses a PEG grammar", "body": "See grammar.peg.",
               "tags": ["parser", "grammar"], "sources": [{"kind": "path", "ref": "src/parser.rs"}]}),
    )
    .await;
    assert_eq!(m1["id"], "MEM-1");
    assert_eq!(m1["key"], "proj/MEM-1");
    assert_eq!(m1["author_name"], "proj/w1");
    assert_eq!(m1["tags"], json!(["grammar", "parser"]));
    assert_eq!(m1["sources"][0]["ref"], "src/parser.rs");
    assert_eq!(m1["similar"], json!([]));

    // The create answer carries the top hits for the title.
    let m2 = mem(
        &td,
        "proj/w2",
        json!({"title": "Parser grammar is LL(1)", "tags": ["parser"]}),
    )
    .await;
    assert_eq!(m2["id"], "MEM-2");
    let similar = ids(&m2["similar"]);
    assert_eq!(similar, ["MEM-1"]);

    let r = td
        .get_as("proj/w1", "/api/projects/proj/memories?q=parser")
        .await;
    let mut got = ids(r.ok());
    got.sort();
    assert_eq!(got, ["MEM-1", "MEM-2"]);
    let r = td
        .get_as("proj/w1", "/api/projects/proj/memories?q=peg")
        .await;
    assert_eq!(ids(r.ok()), ["MEM-1"]);
    let r = td
        .get_as("proj/w1", "/api/projects/proj/memories?tag=grammar")
        .await;
    assert_eq!(ids(r.ok()), ["MEM-1"]);
    let r = td
        .get_as("proj/w1", "/api/projects/proj/memories?author=me")
        .await;
    assert_eq!(ids(r.ok()), ["MEM-1"]);
    // Newest first, paged.
    let r = td.get("/api/projects/proj/memories?limit=1").await;
    assert_eq!(ids(r.ok()), ["MEM-2"]);
    let next = r.next_cursor().unwrap();
    let r = td
        .get(&format!(
            "/api/projects/proj/memories?limit=1&cursor={next}"
        ))
        .await;
    assert_eq!(ids(r.ok()), ["MEM-1"]);

    // Validation.
    td.post_as(
        "proj/w1",
        "/api/projects/proj/memories",
        json!({"title": " "}),
    )
    .await
    .err(422, "invalid");
    td.post_as(
        "proj/w1",
        "/api/projects/proj/memories",
        json!({"title": "x", "sources": [{"kind": "card", "ref": "TER-99"}]}),
    )
    .await
    .err(404, "not_found");

    // Another project cannot read it.
    td.get_as("other/x", "/api/memories/proj%2FMEM-1")
        .await
        .err(403, "forbidden");
    td.get_as("other/x", "/api/projects/proj/memories")
        .await
        .err(403, "forbidden");
    // A bare ID is the agent's own project.
    td.get_as("other/x", "/api/memories/MEM-1")
        .await
        .err(404, "not_found");

    // The operator gives the project when a number is in two projects.
    td.get("/api/memories/MEM-1").await.ok();
    td.post_as(
        "other/x",
        "/api/projects/other/memories",
        json!({"title": "o"}),
    )
    .await
    .ok();
    td.get("/api/memories/MEM-1").await.err(400, "bad_request");
    assert_eq!(
        td.get("/api/memories/other%2FMEM-1").await.ok()["title"],
        "o"
    );

    let ev = td.get("/api/events?kind=memory.created").await;
    assert_eq!(ev.ok().as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn author_edits_anyone_tags_agents_supersede() {
    let td = setup().await;
    mem(
        &td,
        "proj/w1",
        json!({"title": "Builds take 4 minutes", "tags": ["ci"]}),
    )
    .await;
    // Another member may add tags, but not edit.
    td.patch_as("proj/w2", "/api/memories/MEM-1", json!({"title": "no"}))
        .await
        .err(403, "forbidden");
    td.patch_as(
        "proj/w2",
        "/api/memories/MEM-1",
        json!({"remove_tags": ["ci"]}),
    )
    .await
    .err(403, "forbidden");
    let r = td
        .patch_as(
            "proj/w2",
            "/api/memories/MEM-1",
            json!({"add_tags": ["slow"]}),
        )
        .await;
    assert_eq!(r.ok()["tags"], json!(["ci", "slow"]));
    let r = td
        .patch_as(
            "proj/w1",
            "/api/memories/MEM-1",
            json!({"body": "Measured on main."}),
        )
        .await;
    assert_eq!(r.ok()["body"], "Measured on main.");
    // An admin edits any memory; that is audited.
    td.patch_as(
        "proj/lead",
        "/api/memories/MEM-1",
        json!({"body": "Edited."}),
    )
    .await
    .ok();
    let audit = td.get("/api/audit?action=memory.edit").await;
    assert_eq!(audit.ok().as_array().unwrap().len(), 1);

    // Agents never delete; they supersede.
    td.req(Method::DELETE, "/api/memories/MEM-1", None, Some("proj/w1"))
        .await
        .err(403, "forbidden");
    mem(
        &td,
        "proj/w2",
        json!({"title": "Builds take 2 minutes with the cache"}),
    )
    .await;
    let r = td
        .post_as(
            "proj/w2",
            "/api/memories/MEM-1/supersede",
            json!({"by": "MEM-2"}),
        )
        .await;
    let old = r.ok();
    assert_eq!(old["superseded_by"], "MEM-2");
    assert_eq!(old["links"][0]["relation"], "supersedes");
    assert_eq!(old["links"][0]["direction"], "in");
    assert_eq!(old["links"][0]["other"], "MEM-2");
    // Search hides superseded memories unless `all`.
    let r = td
        .get_as("proj/w1", "/api/projects/proj/memories?q=builds")
        .await;
    assert_eq!(ids(r.ok()), ["MEM-2"]);
    let r = td
        .get_as("proj/w1", "/api/projects/proj/memories?q=builds&all=true")
        .await;
    let mut got = ids(r.ok());
    got.sort();
    assert_eq!(got, ["MEM-1", "MEM-2"]);
    let r = td.get_as("proj/w1", "/api/projects/proj/memories").await;
    assert_eq!(ids(r.ok()), ["MEM-2"]);
    // It cannot be superseded back.
    td.post_as(
        "proj/w1",
        "/api/memories/MEM-2/supersede",
        json!({"by": "MEM-1"}),
    )
    .await
    .err(422, "invalid");
    let new = td.get_as("proj/w1", "/api/memories/MEM-2").await;
    assert_eq!(new.ok()["supersedes"], json!(["MEM-1"]));

    // Admins and the operator delete.
    let r = td
        .req(
            Method::DELETE,
            "/api/memories/MEM-1",
            None,
            Some("proj/lead"),
        )
        .await;
    assert_eq!(r.status.as_u16(), 204, "{}", r.body);
    let audit = td.get("/api/audit?action=memory.delete").await;
    assert_eq!(audit.ok().as_array().unwrap().len(), 1);
    td.get("/api/memories/proj%2FMEM-1")
        .await
        .err(404, "not_found");
    let r = td.delete("/api/memories/proj%2FMEM-2").await;
    assert_eq!(r.status.as_u16(), 204);
}

#[tokio::test]
async fn links_contradictions_first_and_graph() {
    let td = setup().await;
    for t in [
        "Use tokio",
        "Use async-std",
        "Async runtime choice",
        "Tokio has io_uring",
    ] {
        mem(&td, "proj/w1", json!({"title": t})).await;
    }
    td.post_as(
        "proj/w2",
        "/api/memories/MEM-3/links",
        json!({"to": "MEM-1", "relation": "relates_to"}),
    )
    .await
    .ok();
    td.post_as(
        "proj/w2",
        "/api/memories/MEM-2/links",
        json!({"to": "MEM-1", "relation": "contradicts", "note": "we disagree"}),
    )
    .await
    .ok();
    let r = td
        .post_as(
            "proj/w2",
            "/api/memories/MEM-4/links",
            json!({"to": "MEM-1", "relation": "supports"}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 201);
    td.post_as(
        "proj/w2",
        "/api/memories/MEM-1/links",
        json!({"to": "MEM-1", "relation": "supports"}),
    )
    .await
    .err(422, "invalid");
    td.post_as(
        "proj/w2",
        "/api/memories/MEM-1/links",
        json!({"to": "MEM-2", "relation": "likes"}),
    )
    .await
    .err(422, "invalid");

    let show = td.get_as("proj/w1", "/api/memories/MEM-1").await;
    let links = show.ok()["links"].as_array().unwrap().clone();
    assert_eq!(links.len(), 3);
    assert_eq!(links[0]["relation"], "contradicts");
    assert_eq!(links[0]["note"], "we disagree");
    assert_eq!(links[0]["author_name"], "proj/w2");

    let g = td
        .get_as("proj/w1", "/api/memories/MEM-3/graph?depth=1")
        .await;
    let g = g.ok();
    let mut nodes = ids(&g["nodes"]);
    nodes.sort();
    assert_eq!(nodes, ["MEM-1", "MEM-3"]);
    let g = td
        .get_as("proj/w1", "/api/memories/MEM-3/graph?depth=2")
        .await;
    let g = g.ok();
    assert_eq!(g["nodes"].as_array().unwrap().len(), 4);
    assert_eq!(g["edges"].as_array().unwrap().len(), 3);
    assert_eq!(g["root"], "MEM-3");

    // Only the link's author or an admin removes it.
    td.req(
        Method::DELETE,
        "/api/memories/MEM-2/links?to=MEM-1&relation=contradicts",
        None,
        Some("proj/w1"),
    )
    .await
    .err(403, "forbidden");
    let r = td
        .req(
            Method::DELETE,
            "/api/memories/MEM-2/links",
            Some(json!({"to": "MEM-1", "relation": "contradicts"})),
            Some("proj/w2"),
        )
        .await;
    assert_eq!(r.ok()["links"], json!([]));

    // A `supersedes` link is the same as supersede.
    td.post_as(
        "proj/w1",
        "/api/memories/MEM-1/links",
        json!({"to": "MEM-2", "relation": "supersedes"}),
    )
    .await
    .ok();
    let r = td.get("/api/memories/proj%2FMEM-2").await;
    assert_eq!(r.ok()["superseded_by"], "MEM-1");
    let ev = td.get("/api/events?kind=memory.linked").await;
    assert_eq!(ev.ok().as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn edits_and_supersedes_isolate_colliding_project_ids() {
    let td = setup().await;
    for (project, agent) in [("proj", "proj/w1"), ("other", "other/x")] {
        for n in 1..=2 {
            td.post_as(
                agent,
                &format!("/api/projects/{project}/memories"),
                json!({"title": format!("{project} title {n}"), "body": format!("{project} body {n}")}),
            )
            .await
            .ok();
        }
    }
    let untouched = td.get("/api/memories/other%2FMEM-1").await.ok().clone();

    // An operator must disambiguate writes just as it does reads.
    td.patch("/api/memories/MEM-1", json!({"body": "ambiguous"}))
        .await
        .err(400, "bad_request");
    td.post("/api/memories/MEM-1/supersede", json!({"by": "MEM-2"}))
        .await
        .err(400, "bad_request");
    // An explicit foreign key cannot redirect an agent's write.
    td.patch_as("proj/w1", "/api/memories/other%2FMEM-1", json!({"body": "foreign"}))
        .await
        .err(403, "forbidden");
    td.post_as("proj/w1", "/api/memories/MEM-1/supersede", json!({"by": "other/MEM-2"}))
        .await
        .err(422, "invalid");

    // Project-qualified operator edits and bare agent edits affect the same row.
    td.patch("/api/memories/proj%2FMEM-1", json!({"body": "operator edit"}))
        .await
        .ok();
    let edited = td.patch_as("proj/w1", "/api/memories/MEM-1", json!({"body": "agent edit"}))
        .await.ok().clone();
    assert_eq!(edited["title"], "proj title 1");
    assert_eq!(edited["body"], "agent edit");
    let old = td.post("/api/memories/proj%2FMEM-1/supersede", json!({"by": "MEM-2"}))
        .await.ok().clone();
    assert_eq!(old["superseded_by"], "MEM-2");
    assert_eq!(td.get("/api/memories/proj%2FMEM-2").await.ok()["supersedes"], json!(["MEM-1"]));
    assert_eq!(td.get("/api/memories/other%2FMEM-1").await.ok(), &untouched);
    let other_new = td.get("/api/memories/other%2FMEM-2").await;
    assert_eq!(other_new.ok()["supersedes"], json!([]));
    assert_eq!(other_new.ok()["body"], "other body 2");
}

#[tokio::test]
async fn same_id_in_two_projects_resolves_by_project() {
    let td = setup().await;
    mem(&td, "proj/w1", json!({"title": "Proj MEM-1 (GPU wheels)"})).await;
    td.post_as(
        "other/x",
        "/api/projects/other/memories",
        json!({"title": "Other MEM-1 (API limit)"}),
    )
    .await
    .ok();

    // Each project's MEM-1, by its key, for the show and the graph route.
    let r = td.get("/api/memories/proj%2FMEM-1").await;
    assert_eq!(r.ok()["title"], "Proj MEM-1 (GPU wheels)");
    let r = td.get("/api/memories/other%2FMEM-1").await;
    assert_eq!(r.ok()["title"], "Other MEM-1 (API limit)");
    let r = td.get("/api/memories/proj%2FMEM-1/graph?depth=1").await;
    assert_eq!(r.ok()["nodes"][0]["title"], "Proj MEM-1 (GPU wheels)");
    let r = td.get("/api/memories/other%2FMEM-1/graph?depth=1").await;
    assert_eq!(r.ok()["nodes"][0]["title"], "Other MEM-1 (API limit)");

    // A bare ID is the caller's own project.
    let r = td.get_as("proj/w1", "/api/memories/MEM-1").await;
    assert_eq!(r.ok()["title"], "Proj MEM-1 (GPU wheels)");
    let r = td.get_as("other/x", "/api/memories/MEM-1").await;
    assert_eq!(r.ok()["title"], "Other MEM-1 (API limit)");

    // A project the caller may not read is refused, for show and graph.
    td.get_as("other/x", "/api/memories/proj%2FMEM-1")
        .await
        .err(403, "forbidden");
    td.get_as("other/x", "/api/memories/proj%2FMEM-1/graph?depth=1")
        .await
        .err(403, "forbidden");
    td.get_as("proj/w1", "/api/memories/other%2FMEM-1")
        .await
        .err(403, "forbidden");
}
