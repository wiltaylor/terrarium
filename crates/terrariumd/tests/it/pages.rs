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

async fn v2_daemon() -> TestDaemon {
    let mut td = TestDaemon::start().await;
    td.web.project_pages_v2 = true;
    td.boot(None, None).await;
    td
}

fn panel(slug: &str, position: usize) -> Value {
    json!({"slug":slug,"title":slug,"position":position,"widgets":[
        {"id":"stable","kind":"board","x":0,"y":0,"w":12,"h":12,
         "frame":"viewport","options":{"view":"lanes","search":"needle"}}
    ]})
}

async fn layout_counts(td: &TestDaemon) -> (i64, i64) {
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .read(|c| {
            Ok((
                c.query_row(
                    "SELECT count(*) FROM audit WHERE action='page.layout'",
                    [],
                    |r| r.get(0),
                )?,
                c.query_row(
                    "SELECT count(*) FROM events WHERE kind='page.updated'",
                    [],
                    |r| r.get(0),
                )?,
            ))
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn v2_off_is_fail_closed_without_seeding_and_keeps_legacy_crud() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let member = td.agent("p", "reader", json!({})).await;
    let reader = member["id"].as_str().unwrap();
    let before = td.get("/api/projects/p/pages").await;
    assert_eq!(before.ok()["capabilities"]["project_pages_v2"], false);
    let path = "/api/projects/p/page-layout";
    td.get(path).await.err(404, "not_found");
    td.put(path, json!({"base_revision":0,"pages":[]}))
        .await
        .err(404, "not_found");
    td.put_as(reader, path, json!({"base_revision":0,"pages":[]}))
        .await
        .err(403, "forbidden");
    for op in ["reset", "copy"] {
        td.post(&format!("{path}/{op}"), json!({}))
            .await
            .err(404, "not_found");
        td.post_as(reader, &format!("{path}/{op}"), json!({}))
            .await
            .err(403, "forbidden");
    }
    assert_eq!(
        td.get("/api/projects/p/pages").await.ok()["items"],
        before.body["items"]
    );
    let mut input = panel("new", 1);
    input.as_object_mut().unwrap().remove("slug");
    td.put("/api/projects/p/pages/new", input)
        .await
        .err(422, "invalid");
    td.put("/api/projects/p/pages/old", body()).await.ok();
    // The collection URL must never reserve a formerly valid custom slug.
    td.put("/api/projects/p/pages/layout", body()).await.ok();
    td.get("/api/projects/p/pages/layout").await.ok();
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .write("test", |t| {
            assert!(terrariumd::store::pages::initialize_defaults(t, "p", "test").is_err());
            Ok(())
        })
        .await
        .unwrap();
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .read(|c| {
            let count: i64 = c.query_row("SELECT count(*) FROM project_page_layouts", [], |r| {
                r.get(0)
            })?;
            assert_eq!(count, 0);
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn v2_atomic_revisions_conflict_validation_and_rights() {
    let td = v2_daemon().await;
    td.project("p").await;
    td.project("q").await;
    let member = td.agent("p", "reader", json!({})).await;
    let admin = td
        .agent("p", "lead", json!({"roles":["project_admin"]}))
        .await;
    let outsider = td
        .agent("q", "other", json!({"roles":["project_admin"]}))
        .await;
    let sys = td.agent("q", "sys", json!({"roles":["sysadmin"]})).await;
    let path = "/api/projects/p/page-layout";
    let input = json!({"base_revision":1,"pages":[panel("one",0)]});
    for id in [
        member["id"].as_str().unwrap(),
        outsider["id"].as_str().unwrap(),
    ] {
        td.put_as(id, path, input.clone())
            .await
            .err(403, "forbidden");
        td.post_as(
            id,
            &format!("{path}/reset"),
            json!({"base_revision":0,"builtin":"board"}),
        )
        .await
        .err(403, "forbidden");
        td.post_as(
            id,
            &format!("{path}/copy"),
            json!({"base_revision":0,"source_project":"q","confirm":true}),
        )
        .await
        .err(403, "forbidden");
    }
    let admin = admin["id"].as_str().unwrap();
    let before = layout_counts(&td).await;
    let (a, b) = tokio::join!(
        td.put_as(admin, path, input.clone()),
        td.put_as(admin, path, input)
    );
    assert_eq!(
        [a.status, b.status].iter().filter(|s| **s == 200).count(),
        1
    );
    let conflict = if a.status == 409 { a } else { b };
    conflict.err(409, "conflict");
    assert_eq!(conflict.body["error"]["details"]["revision"], 2);
    assert_eq!(layout_counts(&td).await, (before.0 + 1, before.1 + 1));
    let saved = td.get(path).await;
    assert_eq!(saved.ok()["revision"], 2);
    for bad in [
        json!({"base_revision":2,"pages":[panel("one",0),panel("one",1)]}),
        json!({"base_revision":2,"pages":[panel("one",2)]}),
        json!({"pages":[]}),
    ] {
        td.put(path, bad).await.err(422, "invalid");
    }
    let mut invalid = panel("two", 1);
    invalid["widgets"][0]["options"]["project"] = json!("q");
    td.put(
        path,
        json!({"base_revision":2,"pages":[panel("valid",0),invalid]}),
    )
    .await
    .err(422, "invalid");
    assert_eq!(td.get(path).await.ok(), saved.ok());
    assert_eq!(layout_counts(&td).await, (before.0 + 1, before.1 + 1));
    td.put_as(
        sys["id"].as_str().unwrap(),
        path,
        json!({"base_revision":2,"pages":[]}),
    )
    .await
    .ok();
    assert_eq!(td.get(path).await.ok()["revision"], 3);
}

#[tokio::test]
async fn v2_bounds_options_and_compatibility_writers() {
    let td = v2_daemon().await;
    td.project("p").await;
    let path = "/api/projects/p/page-layout";
    for (field, value) in [
        ("view", json!("graph")),
        ("search", json!("x".repeat(257))),
        ("decimals", json!(2)),
        ("unknown", json!(true)),
    ] {
        let mut p = panel("bad", 0);
        p["widgets"][0]["options"][field] = value;
        td.put(path, json!({"base_revision":1,"pages":[p]}))
            .await
            .err(422, "invalid");
    }
    let thirty: Vec<_> = (0..30).map(|i| panel(&format!("p{i}"), i)).collect();
    td.put(path, json!({"base_revision":1,"pages":thirty}))
        .await
        .ok();
    assert_eq!(
        td.get("/api/projects/p/pages").await.ok()["items"]
            .as_array()
            .unwrap()
            .len(),
        30
    );
    let too_many: Vec<_> = (0..31).map(|i| panel(&format!("p{i}"), i)).collect();
    td.put(path, json!({"base_revision":2,"pages":too_many}))
        .await
        .err(422, "invalid");
    td.put("/api/projects/p/pages/extra", body())
        .await
        .err(422, "invalid");
    td.put("/api/projects/p/pages/p0", body()).await.ok();
    assert_eq!(td.get(path).await.ok()["revision"], 3);
    td.put(path, json!({"base_revision":2,"pages":[]}))
        .await
        .err(409, "conflict");
    td.delete("/api/projects/p/pages/p29").await.ok();
    assert_eq!(td.get(path).await.ok()["revision"], 4);
    td.put("/api/projects/p/dashboard", json!({"blocks":[]}))
        .await
        .ok();
    assert_eq!(td.get(path).await.ok()["revision"], 5);
    // A direct monitor-removal compatibility write participates too.
    let mut dashboard = body();
    dashboard["widgets"] = json!([{"id":"m","kind":"status","monitor":"missing-local",
        "x":0,"y":0,"w":12,"h":4}]);
    td.put("/api/projects/p/pages/dashboard", dashboard)
        .await
        .ok();
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .write("test", |t| {
            terrariumd::store::pages::drop_dashboard_monitor(t, "p", "missing-local")
        })
        .await
        .unwrap();
    assert_eq!(td.get(path).await.ok()["revision"], 7);
    td.put(path, json!({"base_revision":6,"pages":[]}))
        .await
        .err(409, "conflict");
}

#[tokio::test]
async fn v2_explicit_initializer_preserves_upgrade_collisions_and_deleted_builtins() {
    let mut td = TestDaemon::start().await;
    td.project("p").await;
    td.put("/api/projects/p/pages/board", body()).await.ok();
    // Model the shipped upgrade boundary: ten custom pages plus Dashboard.
    td.daemon.as_ref().unwrap().state.db.write("fixture", |t| {
        for i in 1..10 {
            t.execute("INSERT INTO project_pages(project,slug,title,position,widgets,updated_at,updated_by)
                VALUES('p',?1,'Existing',?2,'[]','before','original')",
                rusqlite::params![format!("existing-{i}"),i])?;
        }
        Ok(())
    }).await.unwrap();
    let custom = td.get("/api/projects/p/pages/board").await.body;
    let dashboard = td.get("/api/projects/p/pages/dashboard").await.body;
    let old_items = td.get("/api/projects/p/pages").await.body["items"].clone();
    assert_eq!(old_items.as_array().unwrap().len(), 11);
    // The enabled start initializes every project before it serves (TER-1077).
    td.web.project_pages_v2 = true;
    td.boot(None, None).await;
    assert_eq!(td.get("/api/projects/p/pages/board").await.ok(), &custom);
    assert_eq!(
        td.get("/api/projects/p/pages/dashboard").await.ok(),
        &dashboard
    );
    let initialized = td.get("/api/projects/p/page-layout").await.body;
    assert_eq!(initialized["revision"], 1);
    assert_eq!(initialized["pages"].as_array().unwrap().len(), 21);
    assert_eq!(
        &initialized["pages"].as_array().unwrap()[..11],
        old_items.as_array().unwrap().as_slice()
    );
    // A project that no initializer has seen yet (written past the store).
    // GET does not seed it, and an interrupted initialization leaves nothing.
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .write("fixture", |t| {
            t.execute(
                "INSERT INTO projects(name,created_at,updated_at) VALUES('raw','x','x')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let raw = td.get("/api/projects/raw/page-layout").await.body;
    assert_eq!(raw["revision"], 0);
    assert_eq!(raw["pages"].as_array().unwrap().len(), 1);
    assert_eq!(td.get("/api/projects/raw/page-layout").await.body, raw);
    let counts = layout_counts(&td).await;
    let interrupted: terrariumd::error::ApiResult<()> = td
        .daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .write("test", |t| {
            terrariumd::store::pages::initialize_defaults(t, "raw", "test")?;
            Err(terrariumd::error::ApiError::invalid(
                "simulated interruption",
            ))
        })
        .await;
    assert!(interrupted.is_err());
    assert_eq!(td.get("/api/projects/raw/page-layout").await.body, raw);
    assert_eq!(layout_counts(&td).await, counts);
    // The next enabled start finishes it, once.
    td.boot(None, None).await;
    let raw_initialized = td.get("/api/projects/raw/page-layout").await.body;
    assert_eq!(raw_initialized["revision"], 1);
    assert_eq!(raw_initialized["pages"].as_array().unwrap().len(), 11);
    assert_eq!(layout_counts(&td).await, (counts.0 + 1, counts.1 + 1));
    assert_eq!(
        td.get("/api/projects/p/page-layout").await.body,
        initialized
    );
    assert_eq!(
        td.get("/api/projects/p/pages/board-1").await.ok()["builtin"],
        "board"
    );
    assert_eq!(
        td.get("/api/projects/p/pages/knowledge").await.ok()["widgets"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    td.delete("/api/projects/p/pages/board-1").await.ok();
    td.boot(None, None).await;
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .write("test", |t| {
            terrariumd::store::pages::initialize_defaults(t, "p", "test")
        })
        .await
        .unwrap();
    td.get("/api/projects/p/pages/board-1")
        .await
        .err(404, "not_found");
    td.post(
        "/api/projects/p/page-layout/reset",
        json!({"base_revision":2,"builtin":"board"}),
    )
    .await
    .ok();
    assert_eq!(td.get("/api/projects/p/pages/board").await.ok(), &custom);
    assert_eq!(
        td.get("/api/projects/p/pages/board-1").await.ok()["builtin"],
        "board"
    );
    // An enabled project creation initializes in the same write (TER-1077).
    td.project("fresh").await;
    let created = td.get("/api/projects/fresh/page-layout").await.body;
    assert_eq!(created["revision"], 1);
    assert_eq!(created["pages"].as_array().unwrap().len(), 11);
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .write("test", |t| {
            terrariumd::store::pages::initialize_defaults(t, "fresh", "test")
        })
        .await
        .unwrap();
    let fresh = td.get("/api/projects/fresh/pages/board").await.body;
    assert_eq!(
        fresh["widgets"],
        td.get("/api/projects/p/pages/board-1").await.body["widgets"]
    );
    let before = td.get("/api/projects/fresh/page-layout").await.body;
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .write("test", |t| {
            terrariumd::store::pages::initialize_defaults(t, "fresh", "test")
        })
        .await
        .unwrap();
    assert_eq!(td.get("/api/projects/fresh/page-layout").await.body, before);
    let inventory = td.get("/api/projects/fresh/pages").await.body["items"].clone();
    td.web.project_pages_v2 = false;
    td.boot(None, None).await;
    td.get("/api/projects/fresh/page-layout")
        .await
        .err(404, "not_found");
    assert_eq!(
        td.get("/api/projects/fresh/pages").await.body["items"],
        inventory
    );
    // Rollback writes: an OFF save of a page with panel or frame widgets is
    // refused (422) and changes nothing, so nothing is lost silently. The
    // legacy dashboard alias of a page without panels still saves.
    let board = td.get("/api/projects/fresh/pages/board").await.body;
    let mut input = board.clone();
    for key in ["project", "slug", "updated_at", "updated_by"] {
        input.as_object_mut().unwrap().remove(key);
    }
    td.put("/api/projects/fresh/pages/board", input)
        .await
        .err(422, "invalid");
    assert_eq!(td.get("/api/projects/fresh/pages/board").await.body, board);
    td.put("/api/projects/fresh/dashboard", json!({"blocks":[]}))
        .await
        .ok();
    let inventory = td.get("/api/projects/fresh/pages").await.body["items"].clone();
    td.web.project_pages_v2 = true;
    td.boot(None, None).await;
    let after = td.get("/api/projects/fresh/page-layout").await.body;
    assert_eq!(after["pages"], inventory);
    assert_eq!(
        after["pages"].as_array().unwrap().len(),
        before["pages"].as_array().unwrap().len()
    );
}

#[tokio::test]
async fn v2_duplicate_instances_and_frame_bounds_round_trip() {
    let td = v2_daemon().await;
    td.project("p").await;
    let path = "/api/projects/p/page-layout";
    let mut page = panel("layout", 0);
    let mut duplicate = page["widgets"][0].clone();
    duplicate["id"] = json!("independent");
    duplicate["y"] = json!(12);
    duplicate["options"]["view"] = json!("done");
    duplicate["options"]["search"] = json!("second");
    page["widgets"].as_array_mut().unwrap().push(duplicate);
    let saved = td
        .put(path, json!({"base_revision":1,"pages":[page.clone()]}))
        .await;
    assert_eq!(saved.ok()["pages"][0]["widgets"], page["widgets"]);
    td.get("/api/projects/p/pages/layout").await.ok();
    for (field, value) in [
        ("w", json!(6)),
        ("frame", json!("unknown")),
        ("id", json!("independent")),
    ] {
        let mut bad = page.clone();
        bad["widgets"][0][field] = value;
        td.put(path, json!({"base_revision":2,"pages":[bad]}))
            .await
            .err(422, "invalid");
    }
    assert_eq!(td.get(path).await.body, saved.body);
}

#[tokio::test]
async fn v2_copy_does_not_grant_foreign_monitor_access() {
    let td = v2_daemon().await;
    td.project("p").await;
    td.project("q").await;
    let reader = td.agent("p", "reader", json!({})).await;
    td.post("/api/hosts", json!({"name":"lab"})).await.ok();
    td.post(
        "/api/targets",
        json!({"name":"box","kind":"host","host":"lab"}),
    )
    .await
    .ok();
    let monitor = td
        .post(
            "/api/projects/q/monitors",
            json!({"name":"source","target":"box",
        "script":"echo 1","kind":"number","schedule":{"kind":"interval","every":"1m"}}),
        )
        .await;
    let id = monitor.ok()["id"].as_str().unwrap();
    let source = json!({"slug":"source","title":"Source","position":0,"widgets":[
        {"id":"metric","kind":"stat","monitor":id,"x":0,"y":0,"w":12,"h":4}
    ]});
    td.put(
        "/api/projects/q/page-layout",
        json!({"base_revision":1,"pages":[source]}),
    )
    .await
    .ok();
    let copied = td
        .post(
            "/api/projects/p/page-layout/copy",
            json!({"base_revision":1,
        "source_project":"q","confirm":true}),
        )
        .await;
    assert_eq!(copied.ok()["pages"][0]["widgets"][0]["monitor"], id);
    let reader = reader["id"].as_str().unwrap();
    td.get_as(reader, &format!("/api/monitors/{id}"))
        .await
        .err(403, "forbidden");
    assert!(
        td.get_as(reader, "/api/projects/p/monitors")
            .await
            .ok()
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["id"] != id)
    );
}

#[tokio::test]
async fn v2_copy_confirmation_source_rights_and_target_local_missing_references() {
    let td = v2_daemon().await;
    td.project("p").await;
    td.project("q").await;
    let admin = td
        .agent("p", "lead", json!({"roles":["project_admin"]}))
        .await;
    let mut source = panel("source", 0);
    source["widgets"] = json!([{"id":"missing","kind":"status","monitor":"local-missing",
        "x":0,"y":0,"w":12,"h":4}]);
    td.put(
        "/api/projects/q/page-layout",
        json!({"base_revision":1,"pages":[source]}),
    )
    .await
    .ok();
    let path = "/api/projects/p/page-layout/copy";
    let input = json!({"base_revision":1,"source_project":"q","confirm":true});
    td.post_as(admin["id"].as_str().unwrap(), path, input.clone())
        .await
        .err(403, "forbidden");
    let before = layout_counts(&td).await;
    td.post(
        path,
        json!({"base_revision":1,"source_project":"q","confirm":false}),
    )
    .await
    .err(422, "invalid");
    assert_eq!(layout_counts(&td).await, before);
    let copied = td.post(path, input.clone()).await;
    assert_eq!(copied.ok()["pages"][0]["project"], "p");
    assert_eq!(
        copied.body["pages"][0]["widgets"][0]["monitor"],
        "local-missing"
    );
    assert_eq!(layout_counts(&td).await, (before.0 + 1, before.1 + 1));
    td.post(path, input).await.err(409, "conflict");
    assert_eq!(
        td.get("/api/projects/q/page-layout").await.ok()["revision"],
        2
    );
    // No source identity/grant is saved in the widget.
    assert!(
        copied.body["pages"][0]["widgets"][0]
            .get("project")
            .is_none()
    );
}

#[tokio::test]
async fn dashboard_alias_reads_writes_and_missing_page() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let path = "/api/projects/p/pages/dashboard";
    let page = td.get(path).await;
    assert_eq!(page.ok()["widgets"].as_array().unwrap().len(), 4);
    let layout = json!({"blocks":[{"id":"note","kind":"note","size":"m","text":"old client"}]});
    td.put("/api/projects/p/dashboard", layout).await.ok();
    let page = td.get(path).await;
    assert_eq!(page.ok()["widgets"][4]["kind"], "markdown");
    assert_eq!(page.body["widgets"][4]["w"], 6);
    let mut input = page.body.clone();
    for field in ["project", "slug", "updated_at", "updated_by"] {
        input.as_object_mut().unwrap().remove(field);
    }
    input["widgets"][4]["text"] = json!("new editor");
    td.put(path, input).await.ok();
    assert_eq!(
        td.get("/api/projects/p/dashboard").await.ok()["blocks"][0]["text"],
        "new editor"
    );
    td.delete(path).await;
    assert_eq!(
        td.get("/api/projects/p/dashboard").await.ok()["blocks"],
        json!([])
    );
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
    let dashboard = "/api/projects/p/pages/dashboard";
    let dashboard_body = json!({"title":"Dashboard","position":0,"widgets":[
        {"id":"wiki","kind":"wiki","x":7,"y":20,"w":5,"h":8,"wiki":"guide","options":{}},
        {"id":"metric","kind":"stat","x":1,"y":5,"w":3,"h":2,"monitor":ids[0]}
    ]});
    td.put(dashboard, dashboard_body.clone()).await.ok();
    td.delete(&format!("/api/monitors/{}", ids[0])).await.ok();
    assert_eq!(td.get(path).await.ok()["widgets"], saved.body["widgets"]);
    assert_eq!(
        td.get(dashboard).await.ok()["widgets"],
        json!([dashboard_body["widgets"][0]])
    );
}

#[tokio::test]
async fn page_list_reports_the_existing_domain_rights() {
    let td = v2_daemon().await;
    td.project("p").await;
    td.project("q").await;
    let member = td.agent("p", "member", json!({})).await;
    let reviewer = td.agent("p", "rev", json!({})).await;
    let lead = td
        .agent("p", "lead", json!({"roles":["project_admin"]}))
        .await;
    let architect = td.agent("q", "arch", json!({"roles":["architect"]})).await;
    let sys = td.agent("q", "sys", json!({"roles":["sysadmin"]})).await;
    let path = "/api/projects/p/pages";
    let rights = |body: &Value| body["rights"].clone();
    // The `reviewer` values that name an agent (policy::review_page).
    let names = |a: &Value, project: &str| {
        let name = a["name"].as_str().unwrap();
        json!([name, a["id"], format!("{project}/{name}")])
    };
    let operator = td.get(path).await;
    assert_eq!(
        rights(operator.ok()),
        json!({"write_project":true,"review_wiki":true,"review_as":[],"admin_project":true,
            "manage_schedules":true,"manage_global":true,"operator":true})
    );
    for (agent, expect) in [
        (
            &member,
            json!({"write_project":true,"review_wiki":false,"review_as":names(&member, "p"),
            "admin_project":false,"manage_schedules":false,"manage_global":false,"operator":false}),
        ),
        (
            &lead,
            json!({"write_project":true,"review_wiki":true,"review_as":names(&lead, "p"),
            "admin_project":true,"manage_schedules":true,"manage_global":false,"operator":false}),
        ),
        // A cross-project reader reads, but has no write right in `p` and no identity.
        (
            &architect,
            json!({"write_project":false,"review_wiki":false,"review_as":[],
            "admin_project":false,"manage_schedules":false,"manage_global":false,"operator":false}),
        ),
        (
            &sys,
            json!({"write_project":true,"review_wiki":true,"review_as":names(&sys, "q"),
            "admin_project":true,"manage_schedules":true,"manage_global":true,"operator":false}),
        ),
    ] {
        let got = td.get_as(agent["id"].as_str().unwrap(), path).await;
        assert_eq!(rights(got.ok()), expect, "{}", agent["name"]);
        assert_eq!(got.body["can_edit"], expect["admin_project"]);
    }
    // The rights only describe the endpoints: the reader's write is still refused.
    td.post_as(
        architect["id"].as_str().unwrap(),
        "/api/projects/p/cards",
        json!({"title":"no"}),
    )
    .await
    .err(403, "forbidden");
    // TER-1157: the review rights match the review endpoint. A member that the
    // page does not name is refused, the named member reviews.
    let page = td
        .post(
            "/api/projects/p/wiki",
            json!({"path":"perf","summary":"new",
                "body":"---\nstatus: ready_for_review\nreviewer: rev\n---\n# Perf\n"}),
        )
        .await;
    let review = format!("/api/wiki/{}/review", page.ok()["id"].as_str().unwrap());
    let got = td.get_as(member["id"].as_str().unwrap(), path).await;
    assert!(
        !rights(got.ok())["review_as"]
            .as_array()
            .unwrap()
            .contains(&json!("rev"))
    );
    td.post_as(
        member["id"].as_str().unwrap(),
        &review,
        json!({"decision":"approve"}),
    )
    .await
    .err(403, "forbidden");
    let got = td.get_as(reviewer["id"].as_str().unwrap(), path).await;
    assert_eq!(rights(got.ok())["review_wiki"], false);
    assert!(
        rights(&got.body)["review_as"]
            .as_array()
            .unwrap()
            .contains(&json!("rev"))
    );
    let done = td
        .post_as(
            reviewer["id"].as_str().unwrap(),
            &review,
            json!({"decision":"approve"}),
        )
        .await;
    assert_eq!(done.ok()["status"], "reviewed");
}

#[tokio::test]
async fn v2_start_refuses_an_over_limit_project_and_initializes_the_others() {
    // TER-1134 (rev-1077 finding 1, `lib.rs` `initialize_project_pages`): a
    // project with 20 stored pages plus Dashboard cannot take the ten
    // built-ins (30 at most). The ON start logs the refusal, leaves that
    // project uninitialized, initializes the others, and still serves.
    let mut td = TestDaemon::start().await;
    td.project("big").await;
    td.project("small").await;
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .write("fixture", |t| {
            for i in 1..=20 {
                t.execute(
                    "INSERT INTO project_pages(project,slug,title,position,widgets,updated_at,updated_by)
                     VALUES('big',?1,'Stored',?2,'[]','before','original')",
                    rusqlite::params![format!("stored-{i}"), i],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
    let big_items = td.get("/api/projects/big/pages").await.body["items"].clone();
    assert_eq!(big_items.as_array().unwrap().len(), 21);
    let counts = layout_counts(&td).await;

    td.web.project_pages_v2 = true;
    td.boot(None, None).await;

    // The over-limit project: no built-ins, no revision, its pages unchanged.
    let big = td.get("/api/projects/big/page-layout").await.body;
    assert_eq!(big["revision"], 0);
    assert_eq!(
        big["pages"].as_array().unwrap(),
        big_items.as_array().unwrap()
    );
    assert!(
        big["pages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p.get("builtin").is_none_or(Value::is_null)),
        "no built-in page on the refused project: {big}"
    );
    // The other project is initialized, once.
    let small = td.get("/api/projects/small/page-layout").await.body;
    assert_eq!(small["revision"], 1);
    assert_eq!(small["pages"].as_array().unwrap().len(), 11);
    assert_eq!(layout_counts(&td).await, (counts.0 + 1, counts.1 + 1));

    // The next start refuses it again and changes nothing.
    td.boot(None, None).await;
    assert_eq!(td.get("/api/projects/big/page-layout").await.body, big);
    assert_eq!(td.get("/api/projects/small/page-layout").await.body, small);
    assert_eq!(layout_counts(&td).await, (counts.0 + 1, counts.1 + 1));
    // The daemon serves both projects.
    td.get("/api/projects").await.ok();
    assert_eq!(
        td.get("/api/projects/big/pages").await.ok()["items"],
        big_items
    );
    td.get("/api/projects/small/pages/board").await.ok();
}
