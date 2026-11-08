//! The DB of a project over the API (PLAN §15.4, docs/api.md §27): the
//! rights of each caller, revisions, refs, events, and audit rows.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::TestDaemon;
use hyper::Method;
use serde_json::{Value, json};

const TYPES: &str = "/api/projects/proj/db/types";
const RECORDS: &str = "/api/projects/proj/db/records";

#[test]
fn template_databases_are_independent_and_restarts_preserve_data() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    common::seed_database(first.path());
    let conn = terrariumd::db::open(&first.path().join("terrarium.db")).unwrap();
    conn.execute_batch(
        "CREATE TABLE test_marker (id INTEGER); INSERT INTO test_marker VALUES (42);",
    )
    .unwrap();
    conn.close().unwrap();
    common::seed_database(second.path());
    common::seed_database(first.path());
    let mut conn = terrariumd::db::open(&first.path().join("terrarium.db")).unwrap();
    assert_eq!(terrariumd::db::migrate(&mut conn).unwrap(), 0);
    assert_eq!(
        conn.query_row("SELECT id FROM test_marker", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        42
    );
    let mut other = terrariumd::db::open(&second.path().join("terrarium.db")).unwrap();
    assert_eq!(terrariumd::db::migrate(&mut other).unwrap(), 0);
    assert!(!other.table_exists(None, "test_marker").unwrap());
    let synchronous: i64 = other
        .pragma_query_value(None, "synchronous", |r| r.get(0))
        .unwrap();
    assert_ne!(synchronous, 0);
}

async fn setup() -> TestDaemon {
    let td = TestDaemon::start().await;
    td.project("proj").await;
    td.agent("proj", "lead", json!({"roles": ["project_admin"]}))
        .await;
    td.agent("proj", "w1", json!({})).await;
    td.project("other").await;
    td.agent("other", "x", json!({})).await;
    td.agent("other", "sys", json!({"roles": ["sysadmin"]}))
        .await;
    td
}

fn service_type() -> Value {
    json!({
        "name": "service",
        "description": "The services we run",
        "fields": [
            {"name": "name", "kind": "string", "required": true, "unique": true},
            {"name": "port", "kind": "integer"}
        ]
    })
}

async fn query(td: &TestDaemon, who: Option<&str>, q: &str) -> common::Resp {
    let path = format!("/api/projects/proj/db/query?q={}", url_encode(q));
    match who {
        Some(a) => td.get_as(a, &path).await,
        None => td.get(&path).await,
    }
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[tokio::test]
async fn rights_of_each_caller() {
    let td = setup().await;
    // An agent of the project makes its own type and records.
    let t = td.post_as("proj/w1", TYPES, service_type()).await;
    assert_eq!(t.ok()["created_by_name"], "proj/w1");
    assert_eq!(t.ok()["revision"], 1);
    let r = td
        .post_as(
            "proj/w1",
            RECORDS,
            json!({"type": "service", "data": {"name": "auth", "port": 8080}}),
        )
        .await;
    let id = r.ok()["id"].as_str().unwrap().to_owned();
    td.post_as(
        "proj/lead",
        RECORDS,
        json!({"type": "service", "data": {"name": "billing"}}),
    )
    .await
    .ok();
    td.post(
        RECORDS,
        json!({"type": "service", "data": {"name": "search"}}),
    )
    .await
    .ok();

    // Members, the lead, the operator, and a sysadmin of another project read.
    for who in [Some("proj/w1"), Some("proj/lead"), None, Some("other/sys")] {
        let r = query(&td, who, "type:service sort:name").await;
        assert_eq!(r.ok()["count"], 3, "{who:?}");
        let r = match who {
            Some(a) => td.get_as(a, TYPES).await,
            None => td.get(TYPES).await,
        };
        assert_eq!(r.ok()[0]["records"], 3);
    }
    // Another project's agent: no access.
    query(&td, Some("other/x"), "type:service")
        .await
        .err(403, "forbidden");
    td.get_as("other/x", TYPES).await.err(403, "forbidden");
    td.get_as("other/x", &format!("{RECORDS}/{id}"))
        .await
        .err(403, "forbidden");
    td.post_as(
        "other/x",
        RECORDS,
        json!({"type": "service", "data": {"name": "evil"}}),
    )
    .await
    .err(403, "forbidden");
    td.post_as(
        "other/x",
        TYPES,
        json!({"name": "x", "fields": [{"name": "a", "kind": "string"}]}),
    )
    .await
    .err(403, "forbidden");
    td.req(
        Method::DELETE,
        &format!("{RECORDS}/{id}"),
        None,
        Some("other/x"),
    )
    .await
    .err(403, "forbidden");
    // A record ID of this project is not found through the other project.
    td.get(&format!("/api/projects/other/db/records/{id}"))
        .await
        .err(404, "not_found");

    // Deleting a type with records: a member may not, the lead may.
    let del = "/api/projects/proj/db/types/service?confirm=records";
    td.req(Method::DELETE, del, None, Some("proj/w1"))
        .await
        .err(403, "forbidden");
    td.req(
        Method::DELETE,
        "/api/projects/proj/db/types/service",
        None,
        Some("proj/lead"),
    )
    .await
    .err(409, "confirm_required");
    td.req(
        Method::DELETE,
        "/api/projects/proj/db/types/service?confirm=true",
        None,
        Some("proj/lead"),
    )
    .await
    .err(409, "confirm_required");
    let r = td.req(Method::DELETE, del, None, Some("proj/lead")).await;
    assert_eq!(r.status.as_u16(), 204, "{}", r.body);
    let audit = td.get("/api/audit?action=db.type.delete").await;
    let rows = audit.ok().as_array().unwrap().clone();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["detail"]["records"], 3);
    // An empty type: any member deletes it with ?confirm=true.
    td.post_as("proj/w1", TYPES, service_type()).await.ok();
    let r = td
        .req(
            Method::DELETE,
            "/api/projects/proj/db/types/service?confirm=true",
            None,
            Some("proj/w1"),
        )
        .await;
    assert_eq!(r.status.as_u16(), 204, "{}", r.body);
}

#[tokio::test]
async fn validation_revisions_refs_and_events() {
    let td = setup().await;
    td.post(TYPES, service_type()).await.ok();
    td.post(
        TYPES,
        json!({"name": "bug", "fields": [
            {"name": "title", "kind": "string", "required": true},
            {"name": "severity", "kind": "enum", "values": ["low", "high"], "default": "low"},
            {"name": "service", "kind": "ref", "ref": "service"}
        ]}),
    )
    .await
    .ok();
    // A bad type: 422 with the problems; a duplicate: 409; a bad body: 400.
    let r = td
        .post(
            TYPES,
            json!({"name": "t", "fields": [{"name": "id", "kind": "string"}]}),
        )
        .await;
    r.err(422, "invalid");
    assert!(
        r.body["error"]["details"]["problems"][0]
            .as_str()
            .unwrap()
            .contains("reserved")
    );
    td.post(TYPES, service_type()).await.err(409, "conflict");
    td.post(TYPES, json!({"name": "t", "fields": [], "extra": 1}))
        .await
        .err(400, "bad_request");

    let svc = td
        .post(
            RECORDS,
            json!({"type": "service", "data": {"name": "auth"}}),
        )
        .await
        .ok()
        .clone();
    let svc_id = svc["id"].as_str().unwrap().to_owned();
    // A ref by name; per-field problems.
    let bug = td
        .post_as(
            "proj/w1",
            RECORDS,
            json!({"type": "bug", "data": {"title": "Login fails", "service": "auth"}}),
        )
        .await
        .ok()
        .clone();
    assert_eq!(bug["data"]["service"], svc_id.as_str());
    assert_eq!(bug["data"]["severity"], "low");
    let r = td
        .post(
            RECORDS,
            json!({"type": "bug", "data": {"service": "nope", "colour": "red"}}),
        )
        .await;
    r.err(422, "invalid");
    let f = &r.body["error"]["details"]["fields"];
    assert_eq!(f["title"], "is required");
    assert!(
        f["service"]
            .as_str()
            .unwrap()
            .contains("no `service` record")
    );
    assert!(f["colour"].as_str().unwrap().contains("is not a field"));
    // A revision conflict on a record.
    let bug_url = format!("{RECORDS}/{}", bug["id"].as_str().unwrap());
    td.patch(
        &bug_url,
        json!({"data": {"severity": "high"}, "revision": 1}),
    )
    .await
    .ok();
    let r = td
        .patch(
            &bug_url,
            json!({"data": {"title": "Old view"}, "revision": 1}),
        )
        .await;
    r.err(409, "conflict");
    assert_eq!(r.body["error"]["details"]["current"], 2);
    assert_eq!(
        r.body["error"]["details"]["record"]["data"]["severity"],
        "high"
    );
    // A revision conflict on a type.
    let r = td
        .patch(
            "/api/projects/proj/db/types/bug",
            json!({"revision": 9, "description": "x"}),
        )
        .await;
    r.err(409, "conflict");
    // The referenced service stays.
    let r = td.delete(&format!("{RECORDS}/{svc_id}")).await;
    r.err(409, "conflict");
    assert_eq!(
        r.body["error"]["details"]["referrers"][0]["field"],
        "service"
    );
    // A bad query: 400 with the position.
    let r = query(&td, None, "type:bug sevrity=high").await;
    r.err(400, "bad_request");
    assert_eq!(r.body["error"]["details"]["position"], 9);
    // A schema change that removes a field needs confirm.
    let r = td
        .patch(
            "/api/projects/proj/db/types/bug",
            json!({"revision": 1, "fields": [
                {"name": "title", "kind": "string", "required": true},
                {"name": "service", "kind": "ref", "ref": "service"}
            ]}),
        )
        .await;
    r.err(409, "confirm_required");
    let r = td
        .patch(
            "/api/projects/proj/db/types/bug",
            json!({"revision": 1, "confirm": true, "fields": [
                {"name": "title", "kind": "string", "required": true},
                {"name": "service", "kind": "ref", "ref": "service"}
            ]}),
        )
        .await;
    assert_eq!(r.ok()["revision"], 2);
    let a = td.get("/api/audit?action=db.type.update").await;
    assert_eq!(a.ok()[0]["detail"]["removed"], json!(["severity"]));

    // Events: project-scoped, with the change. The agent of the other
    // project does not see them.
    let ev = td.get("/api/events?kind=db.record").await;
    let ev = ev.ok().as_array().unwrap().clone();
    assert!(
        ev.iter()
            .any(|e| e["data"]["change"] == "created" && e["data"]["type"] == "bug")
    );
    assert!(ev.iter().any(|e| e["data"]["change"] == "updated"));
    assert!(ev.iter().all(|e| e["project"] == "proj"));
    let created = ev.iter().find(|e| e["data"]["type"] == "bug").unwrap();
    assert_eq!(created["agent"], "w1");
    let ev = td.get("/api/events?kind=db.type").await;
    assert!(
        ev.ok()
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["data"]["change"] == "updated")
    );
    let ev = td.get_as("other/x", "/api/events?kind=db.record").await;
    assert!(ev.ok().as_array().unwrap().is_empty());

    // A record delete is audited.
    let r = td.delete(&bug_url).await;
    assert_eq!(r.status.as_u16(), 204);
    let a = td.get("/api/audit?action=db.record.delete").await;
    assert_eq!(a.ok().as_array().unwrap().len(), 1);
    td.get(&bug_url).await.err(404, "not_found");
}

#[tokio::test]
async fn query_pages_and_shapes() {
    let td = setup().await;
    td.post(TYPES, service_type()).await.ok();
    for i in 0..7 {
        td.post(
            RECORDS,
            json!({"type": "service", "data": {"name": format!("svc{i}"), "port": 8000 + i}}),
        )
        .await
        .ok();
    }
    let r = query(
        &td,
        None,
        r#"type:service port>=8002 sort:-port limit:3 fields:name,created"#,
    )
    .await;
    let v = r.ok();
    assert_eq!(v["count"], 5);
    assert_eq!(v["view"], "table");
    assert_eq!(v["records"].as_array().unwrap().len(), 3);
    assert_eq!(v["records"][0]["data"], json!({"name": "svc6"}));
    assert_eq!(v["fields"][1]["meta"], true);
    assert_eq!(v["next_cursor"], "3");
    let r = td
        .get("/api/projects/proj/db/query?type=service&q=port%3E%3D8002%20sort%3A-port%20limit%3A3&cursor=3")
        .await;
    let names: Vec<&str> = r.ok()["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["data"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["svc3", "svc2"]);
    let r = query(&td, None, "type:service count:true").await;
    assert_eq!(r.ok()["count"], 7);
    assert_eq!(r.ok()["view"], "count");
    let r = query(&td, None, "type:service group:port limit:2").await;
    assert_eq!(r.ok()["groups"].as_array().unwrap().len(), 2);
    assert_eq!(r.ok()["view"], "group");
}
