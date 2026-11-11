//! Persistent dependency waits and their pack action.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common::TestDaemon;
use serde_json::json;
use terrariumd::actor::Actor;
use terrariumd::store::board::card_waits::{self, Action, Dependency, Mode, Wait};

async fn setup() -> TestDaemon {
    let td = TestDaemon::start().await;
    td.post("/api/projects", json!({"name":"p", "card_prefix":"W"}))
        .await
        .ok();
    td.agent("p", "lead", json!({"roles":["project_admin"]}))
        .await;
    td
}

async fn card(td: &TestDaemon) -> String {
    td.post("/api/projects/p/cards", json!({"title":"wait test"}))
        .await
        .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn wait(deps: &[(&str, Option<&str>)], mode: Mode, action: Action) -> Wait {
    Wait {
        dependencies: deps
            .iter()
            .map(|(card, target)| Dependency {
                card: (*card).to_owned(),
                target: target.map(str::to_owned),
            })
            .collect(),
        mode,
        action,
        met_at: None,
    }
}

async fn set(td: &TestDaemon, id: &str, wait: Wait) {
    let id = id.to_owned();
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .write("operator", move |t| {
            card_waits::set(t, &Actor::Operator, &id, &wait)
        })
        .await
        .unwrap();
}

async fn met(td: &TestDaemon, id: &str) -> bool {
    let id = id.to_owned();
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .read(move |c| Ok(card_waits::get(c, &id)?.unwrap().met_at.is_some()))
        .await
        .unwrap()
}

async fn move_to(td: &TestDaemon, id: &str, column: &str) {
    td.post(&format!("/api/cards/{id}/move"), json!({"column":column}))
        .await
        .ok();
}

#[tokio::test]
async fn all_targets_latch_once_and_do_not_undo() {
    let td = setup().await;
    let a = card(&td).await;
    let b = card(&td).await;
    let c = card(&td).await;
    set(
        &td,
        &c,
        wait(
            &[(&a, Some("review")), (&b, Some("working"))],
            Mode::All,
            Action::default(),
        ),
    )
    .await;
    move_to(&td, &a, "review").await;
    assert!(!met(&td, &c).await);
    move_to(&td, &b, "working").await;
    assert!(met(&td, &c).await);
    move_to(&td, &b, "backlog").await;
    move_to(&td, &b, "working").await;
    assert!(met(&td, &c).await);
    let count=td.daemon.as_ref().unwrap().state.db.read(move |db| Ok(db.query_row("SELECT count(*) FROM events WHERE card=?1 AND json_extract(payload,'$.change')='wait_met'", [&c], |r| r.get::<_,i64>(0))?)).await.unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn any_moves_comments_and_wakes_setter_once() {
    let td = setup().await;
    let a = card(&td).await;
    let b = card(&td).await;
    let c = card(&td).await;
    let id = c.clone();
    let w = wait(
        &[(&a, Some("review")), (&b, None)],
        Mode::Any,
        Action {
            move_to: Some("ready".into()),
            comment: Some("dependency ready".into()),
            wake: true,
        },
    );
    let db = td.daemon.as_ref().unwrap().state.db.clone();
    let agent = db
        .read(|conn| Ok(terrariumd::store::agents::find(conn, "lead", Some("p"))?.agent))
        .await
        .unwrap();
    let actor = Actor::Agent(terrariumd::actor::AgentActor {
        id: agent.id.to_string(),
        name: agent.name,
        project: agent.project,
        roles: agent.roles,
        policy: agent.policy,
    });
    db.write("operator", move |t| card_waits::set(t, &actor, &id, &w))
        .await
        .unwrap();
    move_to(&td, &a, "review").await;
    assert!(met(&td, &c).await);
    let result = td.get(&format!("/api/cards/{c}")).await;
    assert_eq!(result.ok()["column"], "ready");
    assert!(
        result.ok()["comments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["body"] == "dependency ready")
    );
    let id = c.clone();
    db.write("operator", move |t| {
        let generation: String = t.query_row(
            "SELECT generation FROM card_waits WHERE card=?1",
            [&id],
            |r| r.get(0),
        )?;
        assert!(!card_waits::act(t, &Actor::Operator, &id, &generation)?);
        Ok(())
    })
    .await
    .unwrap();
    let mail = td.get("/api/mail?to=p/lead").await;
    assert_eq!(
        mail.ok()
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["subject"]
                .as_str()
                .is_some_and(|s| s.contains("wait met")))
            .count(),
        1
    );
}

#[tokio::test]
async fn immediate_completion_default_accepted_and_cycle_refusal() {
    let td = setup().await;
    let a = card(&td).await;
    let b = card(&td).await;
    move_to(&td, &a, "accepted").await;
    set(&td, &b, wait(&[(&a, None)], Mode::All, Action::default())).await;
    assert!(met(&td, &b).await);
    let w = wait(&[(&b, None)], Mode::All, Action::default());
    let db = td.daemon.as_ref().unwrap().state.db.clone();
    let error = db
        .write("operator", move |t| {
            card_waits::set(t, &Actor::Operator, &a, &w)
        })
        .await
        .unwrap_err();
    assert!(error.message.contains("cycle"));
}

#[tokio::test]
async fn archive_breaks_wait_and_notifies_lead_without_moving() {
    let td = setup().await;
    let a = card(&td).await;
    let b = card(&td).await;
    set(
        &td,
        &b,
        wait(
            &[(&a, None)],
            Mode::All,
            Action {
                move_to: Some("ready".into()),
                ..Action::default()
            },
        ),
    )
    .await;
    td.delete(&format!("/api/cards/{a}")).await.ok();
    assert!(met(&td, &b).await);
    let result = td.get(&format!("/api/cards/{b}")).await;
    assert_eq!(result.ok()["column"], "backlog");
    let mail = td.get("/api/mail?to=p/lead").await;
    assert!(mail.ok().as_array().unwrap().iter().any(|m| {
        m["subject"]
            .as_str()
            .is_some_and(|s| s.contains("wait broken"))
    }));
}

#[tokio::test]
async fn refused_move_stays_met_and_wakes_with_reason() {
    let td = setup().await;
    let a = card(&td).await;
    let b = card(&td).await;
    set(
        &td,
        &b,
        wait(
            &[(&a, Some("review"))],
            Mode::All,
            Action {
                move_to: Some("blocked".into()),
                ..Action::default()
            },
        ),
    )
    .await;
    move_to(&td, &a, "review").await;
    assert!(met(&td, &b).await);
    assert_eq!(
        td.get(&format!("/api/cards/{b}")).await.ok()["column"],
        "backlog"
    );
    let mail = td.get("/api/mail?to=p/lead").await;
    assert!(mail.ok().as_array().unwrap().iter().any(|m| {
        m["subject"]
            .as_str()
            .is_some_and(|s| s.contains("move refused"))
    }));
}

#[tokio::test]
async fn cross_project_checks_reader_and_explicit_project() {
    let td = setup().await;
    td.project("other").await;
    let a = td
        .post("/api/projects/other/cards", json!({"title":"other"}))
        .await
        .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let b = card(&td).await;
    let db = td.daemon.as_ref().unwrap().state.db.clone();
    let agent = db
        .read(|conn| Ok(terrariumd::store::agents::find(conn, "lead", Some("p"))?.agent))
        .await
        .unwrap();
    let actor = Actor::Agent(terrariumd::actor::AgentActor {
        id: agent.id.to_string(),
        name: agent.name,
        project: agent.project,
        roles: agent.roles,
        policy: agent.policy,
    });
    let id = b.clone();
    let w = wait(
        &[(&format!("other:{a}"), None)],
        Mode::All,
        Action::default(),
    );
    db.write("operator", move |t| card_waits::set(t, &actor, &id, &w))
        .await
        .unwrap_err();
    set(
        &td,
        &b,
        wait(
            &[(&format!("other:{a}"), Some("review"))],
            Mode::All,
            Action::default(),
        ),
    )
    .await;
    move_to(&td, &a, "review").await;
    assert!(met(&td, &b).await);
}

#[tokio::test]
async fn stale_generation_cannot_act_on_replacement() {
    let td = setup().await;
    let a = card(&td).await;
    let b = card(&td).await;
    set(
        &td,
        &b,
        wait(&[(&a, Some("review"))], Mode::All, Action::default()),
    )
    .await;
    let id = b.clone();
    let db = td.daemon.as_ref().unwrap().state.db.clone();
    let generation = db
        .read(move |c| {
            Ok(c.query_row(
                "SELECT generation FROM card_waits WHERE card=?1",
                [&id],
                |r| r.get::<_, String>(0),
            )?)
        })
        .await
        .unwrap();
    set(
        &td,
        &b,
        wait(
            &[(&a, Some("review"))],
            Mode::All,
            Action {
                move_to: Some("ready".into()),
                ..Action::default()
            },
        ),
    )
    .await;
    let id = b.clone();
    db.write("operator", move |t| {
        assert!(!card_waits::act(t, &Actor::Operator, &id, &generation)?);
        Ok(())
    })
    .await
    .unwrap();
    assert!(!met(&td, &b).await);
}

#[tokio::test]
async fn pack_action_and_latch_survive_daemon_restart() {
    let mut td = setup().await;
    td.patch("/api/projects/p", json!({"board_flow":"rules"}))
        .await
        .ok();
    let a = card(&td).await;
    let b = card(&td).await;
    set(
        &td,
        &b,
        wait(
            &[(&a, Some("review"))],
            Mode::All,
            Action {
                move_to: Some("ready".into()),
                wake: true,
                ..Action::default()
            },
        ),
    )
    .await;
    move_to(&td, &a, "review").await;
    td.daemon.take().unwrap().shutdown();
    td.boot(None, None).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if td.get(&format!("/api/cards/{b}")).await.ok()["column"] == "ready" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "pack action did not move the card"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    td.daemon.take().unwrap().shutdown();
    td.boot(None, None).await;
    assert!(met(&td, &b).await);
    let mail = td.get("/api/mail?to=p/lead").await;
    assert_eq!(
        mail.ok()
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["subject"]
                .as_str()
                .is_some_and(|s| s.contains("wait met")))
            .count(),
        1
    );
}

#[tokio::test]
async fn legacy_dependencies_complete_at_accepted_and_stay_unblocked() {
    let td = setup().await;
    let a = card(&td).await;
    let b = td
        .post(
            "/api/projects/p/cards",
            json!({"title":"legacy", "depends_on":[a]}),
        )
        .await
        .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(!met(&td, &b).await);
    move_to(&td, &a, "accepted").await;
    assert!(met(&td, &b).await);
    move_to(&td, &a, "backlog").await;
    move_to(&td, &b, "working").await;
}

#[tokio::test]
async fn wip_refusal_does_not_move_and_assignee_receives_wake() {
    let td = setup().await;
    td.agent("p", "worker", json!({})).await;
    td.patch(
        "/api/projects/p/board",
        json!({"columns":[
            {"name":"backlog"},{"name":"ready","wip_limit":1},
            {"name":"in_progress","role":"working"},{"name":"done"}
        ]}),
    )
    .await
    .ok();
    let occupied = card(&td).await;
    move_to(&td, &occupied, "ready").await;
    let a = card(&td).await;
    let b = card(&td).await;
    td.post(
        &format!("/api/cards/{b}/assign"),
        json!({"assignee":"worker"}),
    )
    .await
    .ok();
    set(
        &td,
        &b,
        wait(
            &[(&a, Some("working"))],
            Mode::All,
            Action {
                move_to: Some("ready".into()),
                ..Action::default()
            },
        ),
    )
    .await;
    move_to(&td, &a, "working").await;
    assert!(met(&td, &b).await);
    assert_eq!(
        td.get(&format!("/api/cards/{b}")).await.ok()["column"],
        "backlog"
    );
    let mail = td.get("/api/mail?to=p/worker").await;
    assert!(mail.ok().as_array().unwrap().iter().any(|m| {
        m["subject"]
            .as_str()
            .is_some_and(|s| s.contains("WIP limit"))
    }));
}
