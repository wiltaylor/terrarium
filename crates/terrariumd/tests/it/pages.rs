//! Project page rights, bounds, persistence and event/audit contract.
#![allow(clippy::unwrap_used)]
use crate::common::TestDaemon;
use serde_json::{Value, json};

fn body() -> Value {
    json!({"title":"Overview","position":0,"widgets":[
        {"id":"intro","kind":"markdown","x":0,"y":0,"w":6,"h":2,"text":"# Hello","auto_height":true},
        {"id":"wiki","kind":"wiki","x":6,"y":0,"w":6,"h":2,"wiki":"guide/start"}
    ]})
}

#[tokio::test]
async fn dashboard_alias_reads_writes_and_missing_page() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let path = "/api/projects/p/pages/dashboard";
    let page = td.get(path).await;
    assert_eq!(page.ok()["widgets"].as_array().unwrap().len(), 4);
    let layout = json!({"blocks":[{"id":"note","kind":"note","size":"m","text":"old client"}]});
    td.put("/api/projects/p/dashboard",layout).await.ok();
    let page = td.get(path).await;
    assert_eq!(page.ok()["widgets"][4]["kind"], "markdown");
    assert_eq!(page.body["widgets"][4]["w"],6);
    let mut input = page.body.clone();
    for field in ["project","slug","updated_at","updated_by"] { input.as_object_mut().unwrap().remove(field); }
    input["widgets"][4]["text"] = json!("new editor");
    td.put(path,input).await.ok();
    assert_eq!(td.get("/api/projects/p/dashboard").await.ok()["blocks"][0]["text"],"new editor");
    td.delete(path).await;
    assert_eq!(td.get("/api/projects/p/dashboard").await.ok()["blocks"],json!([]));
}

#[tokio::test]
async fn crud_rights_order_limits_and_audit() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.project("q").await;
    let lead = td
        .agent("p", "lead", json!({"roles":["project_admin"]}))
        .await;
    let member = td.agent("p", "member", json!({})).await;
    let outsider = td.agent("q", "other", json!({})).await;
    let sys = td.agent("q", "sys", json!({"roles":["sysadmin"]})).await;
    let lead = lead["id"].as_str().unwrap();
    let member = member["id"].as_str().unwrap();
    let outsider = outsider["id"].as_str().unwrap();
    let sys = sys["id"].as_str().unwrap();
    let path = "/api/projects/p/pages/overview";
    assert_eq!(
        td.get("/api/projects/p/pages").await.ok()["items"][0]["slug"],
        json!("dashboard")
    );
    td.get(path).await.err(404, "not_found");
    td.get("/api/projects/missing/pages")
        .await
        .err(404, "not_found");
    td.put_as(member, path, body()).await.err(403, "forbidden");
    assert_eq!(
        td.get_as(member, "/api/projects/p/pages").await.ok()["can_edit"],
        false
    );
    assert_eq!(
        td.get_as(lead, "/api/projects/p/pages").await.ok()["can_edit"],
        true
    );
    td.put_as(outsider, path, body())
        .await
        .err(403, "forbidden");
    let r = td.put_as(lead, path, body()).await;
    assert_eq!(r.ok()["slug"], "overview");
    assert_eq!(r.body["project"], "p");
    assert!(r.body["updated_at"].is_string());
    assert!(r.body["updated_by"].is_string());
    assert_eq!(
        td.get_as(member, path).await.ok()["widgets"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    td.get_as(outsider, path).await.err(403, "forbidden");
    td.put_as(sys, path, body()).await.ok();
    for n in 1..9 {
        let mut b = body();
        b["position"] = json!(n);
        td.put(&format!("/api/projects/p/pages/page{n}"), b)
            .await
            .ok();
    }
    td.put("/api/projects/p/pages/eleventh", body())
        .await
        .err(422, "invalid");
    let mut b = body();
    b["title"] = json!("Renamed");
    b["position"] = json!(20);
    td.put(path, b).await.ok(); // Replacement still works at the limit.
    let list = td.get("/api/projects/p/pages").await;
    assert_eq!(list.ok()["items"][9]["title"], "Renamed");
    td.delete_as(member, path).await.err(403, "forbidden");
    assert_eq!(td.delete_as(lead, path).await.status, 204);
    td.get(path).await.err(404, "not_found");
    td.delete(path).await.err(404, "not_found");
    td.put(path, body()).await.ok();
    let state = td.daemon.as_ref().unwrap().state.clone();
    state
        .db
        .read(|c| {
            let audits: i64 = c.query_row(
                "SELECT count(*) FROM audit WHERE action IN ('page.set','page.remove')",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(audits, 13);
            let events: i64 = c.query_row(
                "SELECT count(*) FROM events WHERE kind='page.updated'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(events, 13);
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn rejects_invalid_layouts_and_accepts_boundary_cells() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let path = "/api/projects/p/pages/overview";
    for (field, value) in [
        ("x", json!(12)),
        ("x", json!(-1)),
        ("x", json!(0.5)),
        ("w", json!(0)),
        ("w", json!(13)),
        ("h", json!(0)),
        ("y", json!(10000)),
        ("h", json!(u32::MAX)),
        ("kind", json!("bogus")),
        ("id", json!("")),
        ("text", json!("é".repeat(8193))),
    ] {
        let mut b = body();
        b["widgets"][0][field] = value;
        td.put(path, b).await.err(422, "invalid");
    }
    for (field, value) in [
        ("x", json!(5)),
        ("id", json!("intro")),
        ("wiki", json!("../q")),
        ("monitor", json!("foreign")),
    ] {
        let mut b = body();
        b["widgets"][1][field] = value;
        td.put(path, b).await.err(422, "invalid");
    }
    for (field, value) in [
        ("title", json!(" ")),
        ("position", json!(-1)),
        ("unknown", json!(true)),
        ("widgets", json!(vec![body()["widgets"][0].clone(); 101])),
    ] {
        let mut b = body();
        b[field] = value;
        td.put(path, b).await.err(422, "invalid");
    }
    for slug in ["UPPER", "-bad", "bad.dot", &"x".repeat(65)] {
        td.put(&format!("/api/projects/p/pages/{slug}"), body())
            .await
            .err(422, "invalid");
    }
    let mut b = body();
    b["widgets"] = json!([{"id":"edge","kind":"markdown","x":11,"y":9999,"w":1,"h":1,"text":"é".repeat(8192)}]);
    td.put(path, b).await.ok();
    let mut b = body();
    b["widgets"] = json!([]);
    td.put(path, b).await.ok();
}

#[tokio::test]
async fn monitor_sources_are_local_compatible_and_preserved_when_deleted() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.project("q").await;
    let host = td.post("/api/hosts", json!({"name":"lab"})).await;
    host.ok();
    td.post(
        "/api/targets",
        json!({"name":"box","kind":"host","host":"lab"}),
    )
    .await
    .ok();
    let mut ids = Vec::new();
    for (project, kind) in [
        ("p", "number"),
        ("p", "json"),
        ("p", "text"),
        ("q", "number"),
    ] {
        let r=td.post(&format!("/api/projects/{project}/monitors"),json!({"name":kind,"target":"box","script":"echo 1","kind":kind,"schedule":{"kind":"interval","every":"1m"}})).await;
        ids.push(r.ok()["id"].as_str().unwrap().to_owned());
    }
    let path = "/api/projects/p/pages/overview";
    let mut b = body();
    for (kind, id) in [
        ("stat", &ids[0]),
        ("chart", &ids[0]),
        ("status", &ids[2]),
        ("text", &ids[2]),
        ("table", &ids[1]),
    ] {
        b["widgets"] = json!([{"id":"metric","kind":kind,"x":0,"y":0,"w":12,"h":2,"monitor":id}]);
        td.put(path, b.clone()).await.ok();
    }
    for (kind, id) in [
        ("stat", &ids[2]),
        ("chart", &ids[2]),
        ("table", &ids[0]),
        ("stat", &ids[3]),
        ("stat", &"missing".to_owned()),
    ] {
        b["widgets"] = json!([{"id":"metric","kind":kind,"x":0,"y":0,"w":12,"h":2,"monitor":id}]);
        td.put(path, b.clone()).await.err(422, "invalid");
    }
    b["widgets"] = json!([{"id":"metric","kind":"stat","x":0,"y":0,"w":12,"h":2,"monitor":ids[0]}]);
    let saved = td.put(path, b.clone()).await;
    saved.ok();
    td.delete(&format!("/api/monitors/{}", ids[0])).await.ok();
    assert_eq!(td.get(path).await.ok()["widgets"], saved.body["widgets"]);
}
