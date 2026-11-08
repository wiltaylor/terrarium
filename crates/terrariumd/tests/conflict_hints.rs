//! Conflict checks against a real local remote and the daemon database.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common::TestDaemon;
use serde_json::json;

#[tokio::test]
async fn branch_overlap_is_visible_to_both_cards_and_lead_without_redundant_fetch() {
    let remote = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git").arg("-C").arg(remote.path()).args(args).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    };
    git(&["init", "-b", "next"]);
    git(&["config", "user.name", "Test"]);
    git(&["config", "user.email", "test@example.invalid"]);
    git(&["commit", "--allow-empty", "-m", "base"]);
    for branch in ["a", "b"] {
        git(&["checkout", "-B", branch, "next"]);
        std::fs::write(remote.path().join("shared.rs"), branch).unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "branch change"]);
    }
    let td = TestDaemon::start().await;
    td.project("p").await;
    let lead = td.agent("p", "lead", json!({"roles": ["project_admin"]})).await;
    let repo = td.post("/api/projects/p/repos", json!({"name": "app", "url": remote.path().to_str().unwrap()})).await.ok().clone();
    let state = td.daemon.as_ref().unwrap().state.clone();
    state.db.write("operator", |t| {
        let def = serde_json::from_value(json!({"next_repos": ["app"]})).unwrap();
        terrariumd::store::jobs::set_delivery(t, "p", &def)?;
        Ok(())
    }).await.unwrap();
    let mut ids = Vec::new();
    for branch in ["a", "b", "not-pushed"] {
        let card = td.post("/api/projects/p/cards", json!({"title": branch, "repo": "app", "branch": branch})).await.ok().clone();
        let id = card["id"].as_str().unwrap().to_owned();
        let c = id.clone();
        state.db.write("operator", move |t| {
            t.execute("UPDATE cards SET column_id = (SELECT id FROM board_columns WHERE project = 'p' AND role = 'working') WHERE id = ?1", [c])?;
            Ok(())
        }).await.unwrap();
        ids.push(id);
    }
    terrariumd::conflict_hints::pass(&state).await;
    for id in &ids[..2] {
        let card = td.get(&format!("/api/cards/{id}")).await.ok().clone();
        assert!(card["comments"].as_array().unwrap().iter().any(|c| c["body"].as_str().unwrap().contains("shared.rs")));
    }
    let unpushed = td.get(&format!("/api/cards/{}", ids[2])).await.ok().clone();
    assert!(unpushed["comments"].as_array().unwrap().is_empty());
    let mail = td.mail_of(lead["id"].as_str().unwrap()).await;
    assert_eq!(mail.iter().filter(|m| m["subject"].as_str().unwrap().starts_with("Conflict hint:")).count(), 1);
    let mirror = state.state_dir.join("repo-mirrors").join(format!("{}.git", repo["id"].as_str().unwrap()));
    let before = std::fs::metadata(mirror.join("FETCH_HEAD")).ok().map(|m| m.modified().unwrap());
    terrariumd::conflict_hints::pass(&state).await;
    let after = std::fs::metadata(mirror.join("FETCH_HEAD")).ok().map(|m| m.modified().unwrap());
    assert_eq!(before, after, "unchanged remote refs must not trigger another fetch");
    let mail = td.mail_of(lead["id"].as_str().unwrap()).await;
    assert_eq!(mail.iter().filter(|m| m["subject"].as_str().unwrap().starts_with("Conflict hint:")).count(), 1);
}
