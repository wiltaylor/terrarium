//! Project folders (docs/api.md §6.12, TER-48): create and nest, rename,
//! move and reorder, a move into itself or a subfolder, the depth limit,
//! the place of a project, a delete that moves the content up one level,
//! the cascade when a project is deleted, and the rights (the operator
//! and a sysadmin).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::TestDaemon;
use serde_json::{Value, json};

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

/// `name(parent name)@position` for each folder, in list order.
fn folders(tree: &Value) -> Vec<String> {
    let all = tree["folders"].as_array().unwrap();
    let name_of = |id: &Value| {
        all.iter()
            .find(|f| &f["id"] == id)
            .map(|f| f["name"].as_str().unwrap().to_owned())
            .unwrap_or_default()
    };
    let mut rows: Vec<String> = all
        .iter()
        .map(|f| {
            format!(
                "{}({})@{}",
                f["name"].as_str().unwrap(),
                name_of(&f["parent_id"]),
                f["position"]
            )
        })
        .collect();
    rows.sort();
    rows
}

/// `project@folder id or -:position` for each placement, sorted.
fn places(tree: &Value) -> Vec<String> {
    let mut rows: Vec<String> = tree["placements"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            format!(
                "{}@{}:{}",
                p["project"].as_str().unwrap(),
                p["folder_id"].as_str().unwrap_or("-"),
                p["position"]
            )
        })
        .collect();
    rows.sort();
    rows
}

#[tokio::test]
async fn folders_nest_move_and_delete_up() {
    let td = TestDaemon::start().await;
    td.project("alpha").await;
    td.project("beta").await;
    td.project("gamma").await;

    assert_eq!(
        td.get("/api/project-folders").await.ok(),
        &json!({"folders": [], "placements": []})
    );

    // Create at the root and nest. Names are trimmed.
    let r = td
        .post("/api/project-folders", json!({"name": "  Work "}))
        .await;
    assert_eq!(r.status.as_u16(), 201, "{}", r.body);
    assert_eq!(r.body["name"], "Work");
    assert_eq!(r.body["parent_id"], Value::Null);
    let work = id(&r.body);
    let home = id(td
        .post("/api/project-folders", json!({"name": "Home"}))
        .await
        .ok());
    let infra = id(td
        .post(
            "/api/project-folders",
            json!({"name": "Infra", "parent_id": work}),
        )
        .await
        .ok());
    let tree = td.get("/api/project-folders").await;
    assert_eq!(
        folders(tree.ok()),
        ["Home()@1", "Infra(Work)@0", "Work()@0"]
    );

    // Bad names and an unknown parent.
    td.post("/api/project-folders", json!({"name": " "}))
        .await
        .err(422, "invalid");
    td.post("/api/project-folders", json!({"name": "a\u{7}b"}))
        .await
        .err(422, "invalid");
    td.post(
        "/api/project-folders",
        json!({"name": "x", "parent_id": "nope"}),
    )
    .await
    .err(404, "not_found");

    // Rename, and reorder at the root.
    let r = td
        .patch(
            &format!("/api/project-folders/{home}"),
            json!({"name": "Personal", "position": 0}),
        )
        .await;
    assert_eq!(r.ok()["name"], "Personal");
    assert_eq!(
        folders(td.get("/api/project-folders").await.ok()),
        ["Infra(Work)@0", "Personal()@0", "Work()@1"]
    );

    // A move into itself or a subfolder is refused.
    td.patch(
        &format!("/api/project-folders/{work}"),
        json!({"parent_id": infra}),
    )
    .await
    .err(422, "invalid");
    td.patch(
        &format!("/api/project-folders/{work}"),
        json!({"parent_id": work}),
    )
    .await
    .err(422, "invalid");

    // Move Personal under Infra, then back to the root with null.
    let r = td
        .patch(
            &format!("/api/project-folders/{home}"),
            json!({"parent_id": infra}),
        )
        .await;
    assert_eq!(r.ok()["parent_id"], json!(infra));
    assert_eq!(
        folders(td.get("/api/project-folders").await.ok()),
        ["Infra(Work)@0", "Personal(Infra)@0", "Work()@0"]
    );
    td.patch(
        &format!("/api/project-folders/{home}"),
        json!({"parent_id": null}),
    )
    .await
    .ok();
    assert_eq!(
        folders(td.get("/api/project-folders").await.ok()),
        ["Infra(Work)@0", "Personal()@1", "Work()@0"]
    );

    // Place projects. A second place moves; null is the root.
    let r = td
        .put("/api/projects/alpha/folder", json!({"folder_id": infra}))
        .await;
    assert_eq!(
        r.ok(),
        &json!({"project": "alpha", "folder_id": infra, "position": 0})
    );
    td.put("/api/projects/beta/folder", json!({"folder_id": infra}))
        .await
        .ok();
    td.put("/api/projects/gamma/folder", json!({"folder_id": work}))
        .await
        .ok();
    // Reorder: beta first.
    td.put(
        "/api/projects/beta/folder",
        json!({"folder_id": infra, "position": 0}),
    )
    .await
    .ok();
    assert_eq!(
        places(td.get("/api/project-folders").await.ok()),
        [
            format!("alpha@{infra}:1"),
            format!("beta@{infra}:0"),
            format!("gamma@{work}:0"),
        ]
    );
    td.put("/api/projects/nope/folder", json!({"folder_id": infra}))
        .await
        .err(404, "not_found");
    td.put("/api/projects/alpha/folder", json!({"folder_id": "nope"}))
        .await
        .err(404, "not_found");

    // Delete Infra: its projects go to Work after gamma, in their order.
    let r = td.delete(&format!("/api/project-folders/{infra}")).await;
    assert_eq!(r.status.as_u16(), 204, "{}", r.body);
    let tree = td.get("/api/project-folders").await;
    assert_eq!(folders(tree.ok()), ["Personal()@1", "Work()@0"]);
    assert_eq!(
        places(tree.ok()),
        [
            format!("alpha@{work}:2"),
            format!("beta@{work}:1"),
            format!("gamma@{work}:0"),
        ]
    );

    // A subfolder moves up at the place of the deleted folder.
    let sub = id(td
        .post(
            "/api/project-folders",
            json!({"name": "Sub", "parent_id": work}),
        )
        .await
        .ok());
    td.delete(&format!("/api/project-folders/{work}"))
        .await
        .ok();
    let tree = td.get("/api/project-folders").await;
    assert_eq!(folders(tree.ok()), ["Personal()@1", "Sub()@0"]);
    assert_eq!(places(tree.ok()), ["alpha@-:2", "beta@-:1", "gamma@-:0"]);
    td.delete(&format!("/api/project-folders/{work}"))
        .await
        .err(404, "not_found");

    // A deleted project loses its place.
    td.put("/api/projects/alpha/folder", json!({"folder_id": sub}))
        .await
        .ok();
    let r = td.delete("/api/projects/alpha").await;
    assert!(r.status.is_success(), "{}", r.body);
    assert_eq!(
        places(td.get("/api/project-folders").await.ok()),
        ["beta@-:1", "gamma@-:0"]
    );

    let audit = td.get("/api/audit?order=desc&limit=200").await;
    let text = audit.body.to_string();
    for action in [
        "project_folder.create",
        "project_folder.update",
        "project_folder.delete",
        "project.folder",
    ] {
        assert!(text.contains(action), "{action}: {text}");
    }
}

#[tokio::test]
async fn folders_nest_at_most_eight_levels() {
    let td = TestDaemon::start().await;
    let mut parent = Value::Null;
    let mut ids = Vec::new();
    for n in 1..=8 {
        let r = td
            .post(
                "/api/project-folders",
                json!({"name": format!("L{n}"), "parent_id": parent}),
            )
            .await;
        let f = id(r.ok());
        parent = json!(f);
        ids.push(f);
    }
    td.post(
        "/api/project-folders",
        json!({"name": "L9", "parent_id": parent}),
    )
    .await
    .err(422, "invalid");

    // A folder with one subfolder cannot go under level 7.
    let a = id(td
        .post("/api/project-folders", json!({"name": "A"}))
        .await
        .ok());
    td.post("/api/project-folders", json!({"name": "B", "parent_id": a}))
        .await
        .ok();
    td.patch(
        &format!("/api/project-folders/{a}"),
        json!({"parent_id": ids[6]}),
    )
    .await
    .err(422, "invalid");
    td.patch(
        &format!("/api/project-folders/{a}"),
        json!({"parent_id": ids[5]}),
    )
    .await
    .ok();
}

#[tokio::test]
async fn only_the_operator_and_a_sysadmin_see_folders() {
    let td = TestDaemon::start().await;
    td.project("ops").await;
    let dev = id(&td.agent("ops", "dev", json!({})).await);
    let sys = id(&td.agent("ops", "sys", json!({"roles": ["sysadmin"]})).await);

    td.get_as(&dev, "/api/project-folders")
        .await
        .err(403, "forbidden");
    td.post_as(&dev, "/api/project-folders", json!({"name": "x"}))
        .await
        .err(403, "forbidden");
    td.put_as(&dev, "/api/projects/ops/folder", json!({"folder_id": null}))
        .await
        .err(403, "forbidden");

    let f = id(td
        .post_as(&sys, "/api/project-folders", json!({"name": "Ops"}))
        .await
        .ok());
    td.patch_as(
        &dev,
        &format!("/api/project-folders/{f}"),
        json!({"name": "y"}),
    )
    .await
    .err(403, "forbidden");
    td.put_as(&sys, "/api/projects/ops/folder", json!({"folder_id": f}))
        .await
        .ok();
    let tree = td.get_as(&sys, "/api/project-folders").await;
    assert_eq!(places(tree.ok()), [format!("ops@{f}:0")]);
}
