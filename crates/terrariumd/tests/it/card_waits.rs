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

#[tokio::test]
async fn api_wait_set_show_clear_and_atomic_card_writes() {
    let td = setup().await;
    let dep = card(&td).await;
    let id = card(&td).await;
    let path = format!("/api/cards/{id}/wait-on");
    assert!(td.get(&path).await.ok().is_null());
    let body = json!({"dependencies":[{"card":dep,"target":"review"}],"mode":"any","action":{"wake":true}});
    let configured = td.put(&path, body.clone()).await;
    assert_eq!(configured.ok()["dependencies"][0]["met"], false);
    assert!(configured.ok()["generation"].is_string());
    assert!(configured.ok()["wake_recipient"].is_string());
    assert_eq!(configured.ok()["dependencies"][0]["hidden"], false);
    let board = td.get("/api/projects/p/board").await;
    let tile = board.ok()["cards"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap();
    assert_eq!(
        tile["wait"],
        json!({"mode":"any","met":false,"deps_met":0,"deps_total":1})
    );
    assert_eq!(
        td.get(&format!("/api/cards/{id}")).await.ok()["wait"],
        *configured.ok()
    );
    td.post(
        &format!("/api/cards/{dep}/move"),
        json!({"column":"review"}),
    )
    .await
    .ok();
    let met = td.get(&path).await;
    assert_eq!(met.ok()["dependencies"][0]["met"], true);
    assert!(met.ok()["met_at"].is_string());
    td.delete(&path).await.ok();
    assert!(td.get(&path).await.ok().is_null());
    assert_eq!(
        td.get(&format!("/api/cards/{id}")).await.ok()["depends_on"],
        json!([])
    );
    let created = td
        .post(
            "/api/projects/p/cards",
            json!({"title":"new wait","wait":body}),
        )
        .await;
    let new_id = created.ok()["id"].as_str().unwrap();
    assert_eq!(
        td.get(&format!("/api/cards/{new_id}/wait-on")).await.ok()["mode"],
        "any"
    );
    td.patch(&format!("/api/cards/{id}"), json!({"wait":body}))
        .await
        .ok();
    assert_eq!(td.get(&path).await.ok()["mode"], "any");
    let failed = td
        .patch(
            &format!("/api/cards/{id}"),
            json!({"title":"must roll back","wait":{"dependencies":[]}}),
        )
        .await;
    assert_eq!(failed.status.as_u16(), 422);
    assert_eq!(
        td.get(&format!("/api/cards/{id}")).await.ok()["title"],
        "wait test"
    );
    let conflict = td
        .patch(
            &format!("/api/cards/{id}"),
            json!({"depends_on":[],"wait":body}),
        )
        .await;
    assert_eq!(conflict.status.as_u16(), 422);
    let pending = card(&td).await;
    let failed = td
        .post(
            "/api/projects/p/cards",
            json!({"title":"blocked initial move","column":"working",
        "wait":{"dependencies":[{"card":pending}]}}),
        )
        .await;
    assert_eq!(failed.status.as_u16(), 409);
}

#[tokio::test]
async fn api_wait_permissions_and_cross_project_redaction() {
    let td = setup().await;
    let agent = td.agent("p", "worker", json!({})).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    td.post("/api/projects", json!({"name":"secret","card_prefix":"S"}))
        .await
        .ok();
    let secret = td
        .post("/api/projects/secret/cards", json!({"title":"private"}))
        .await
        .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let own = td
        .post_as(&agent, "/api/projects/p/cards", json!({"title":"mine"}))
        .await
        .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let path = format!("/api/cards/{own}/wait-on");
    let body = json!({"dependencies":[{"card":format!("secret:{secret}"),"target":"done"}]});
    assert_eq!(
        td.put_as(&agent, &path, body.clone()).await.status.as_u16(),
        403
    );
    td.put(&path, body).await.ok();
    let hidden = td.get_as(&agent, &path).await;
    assert_eq!(
        hidden.ok()["dependencies"][0],
        json!({"card":null,"hidden":true,"met":false})
    );
    assert_eq!(
        td.get_as(&agent, &format!("/api/cards/{secret}/wait-on"))
            .await
            .status
            .as_u16(),
        403
    );
    let foreign = card(&td).await;
    assert_eq!(
        td.put_as(
            &agent,
            &format!("/api/cards/{foreign}/wait-on"),
            json!({"dependencies":[{"card":own}]})
        )
        .await
        .status
        .as_u16(),
        403
    );
    assert_eq!(
        td.delete_as(&agent, &format!("/api/cards/{foreign}/wait-on"))
            .await
            .status
            .as_u16(),
        403
    );
    td.delete_as(&agent, &path).await.ok();
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
async fn calendar_freezes_refuse_once_without_changing_manual_moves() {
    for scope in [
        "/api/projects/p/calendar/events",
        "/api/calendar/global/events",
    ] {
        let td = setup().await;
        let at = chrono::Utc::now();
        let event = td
            .post(
                scope,
                json!({
                    "title":"release freeze", "kind":"freeze", "tz":"UTC",
                    "start":(at - chrono::Duration::hours(1)).to_rfc3339(),
                    "end":(at + chrono::Duration::hours(1)).to_rfc3339()
                }),
            )
            .await
            .ok()["id"]
            .as_str()
            .unwrap()
            .to_owned();
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
        assert!(
            td.get("/api/mail?to=p/lead")
                .await
                .ok()
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["subject"]
                    .as_str()
                    .is_some_and(|s| s.contains("calendar freeze")))
        );
        // Manual moves remain allowed even during the freeze.
        move_to(&td, &b, "ready").await;
        move_to(&td, &b, "backlog").await;
        td.delete(&format!("/api/calendar/events/{event}"))
            .await
            .ok();
        move_to(&td, &a, "backlog").await;
        move_to(&td, &a, "review").await;
        assert!(met(&td, &b).await);
        let id = b.clone();
        td.daemon.as_ref().unwrap().state.db.write("operator", move |t| {
            let generation: String = t.query_row("SELECT generation FROM card_waits WHERE card=?1", [&id], |r| r.get(0))?;
            assert!(!card_waits::act(t, &Actor::Operator, &id, &generation)?);
            let audited: bool = t.query_row("SELECT EXISTS(SELECT 1 FROM audit WHERE action='card.wait_action_failed' AND target=?1)", [&id], |r| r.get(0))?;
            assert!(audited);
            Ok(())
        }).await.unwrap();
        assert_eq!(
            td.get(&format!("/api/cards/{b}")).await.ok()["column"],
            "backlog"
        );
    }
}

#[tokio::test]
async fn unrelated_and_expired_freezes_do_not_block_wait_moves() {
    let td = setup().await;
    td.project("other").await;
    let at = chrono::Utc::now();
    for (scope, hours) in [
        ("/api/projects/other/calendar/events", 1),
        ("/api/projects/p/calendar/events", -1),
    ] {
        td.post(
            scope,
            json!({
                "title":"freeze", "kind":"freeze", "tz":"UTC",
                "start":(at - chrono::Duration::hours(2)).to_rfc3339(),
                "end":(at + chrono::Duration::hours(hours)).to_rfc3339()
            }),
        )
        .await
        .ok();
    }
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
                ..Action::default()
            },
        ),
    )
    .await;
    move_to(&td, &a, "review").await;
    assert!(met(&td, &b).await);
    assert_eq!(
        td.get(&format!("/api/cards/{b}")).await.ok()["column"],
        "ready"
    );
    td.daemon.as_ref().unwrap().state.db.read(|c| {
        for scope in [Some("p"), None] {
            let mut stmt = c.prepare("EXPLAIN QUERY PLAN SELECT id FROM calendar_events WHERE project IS ?1 ORDER BY start_at, id LIMIT 1001")?;
            let plan = stmt.query_map([scope], |r| r.get::<_, String>(3))?.collect::<Result<Vec<_>, _>>()?;
            assert!(plan.iter().any(|p| p.contains("SEARCH calendar_events USING INDEX calendar_events_project")), "{plan:?}");
        }
        Ok(())
    }).await.unwrap();
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
async fn reopened_dependency_resets_only_unacted_waits() {
    let td = setup().await;
    let dep = card(&td).await;
    let pending = card(&td).await;
    let acted = card(&td).await;
    for id in [&pending, &acted] {
        set(&td, id, wait(&[(&dep, None)], Mode::All, Action::default())).await;
    }
    move_to(&td, &dep, "accepted").await;
    let p = pending.clone();
    // Model completion queued for the rules action, including a seeded legacy wait.
    td.daemon.as_ref().unwrap().state.db.write("operator", move |t| {
        t.execute("UPDATE card_waits SET acted_at=NULL WHERE card=?1", [&p])?;
        Ok(())
    }).await.unwrap();
    move_to(&td, &dep, "done").await;
    assert!(met(&td, &pending).await);
    move_to(&td, &dep, "working").await;
    assert!(!met(&td, &pending).await);
    assert!(met(&td, &acted).await);
    for id in [&pending, &acted] {
        let result = td.get(&format!("/api/cards/{id}")).await;
        assert!(result.ok()["comments"].as_array().unwrap().iter().any(|c|
            c["body"].as_str().is_some_and(|s| s.contains("moved back out"))));
        assert_eq!(result.ok()["blocked_by"], json!([dep]));
    }
    move_to(&td, &dep, "accepted").await;
    assert!(met(&td, &pending).await);
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

#[tokio::test]
async fn held_move_refusal_notifies_and_comment_only_action_emits_comment() {
    let td = setup().await;
    let a = card(&td).await;
    let b = card(&td).await;
    let c = card(&td).await;
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
    set(
        &td,
        &c,
        wait(
            &[(&a, Some("review"))],
            Mode::All,
            Action {
                comment: Some("ready for work".into()),
                ..Action::default()
            },
        ),
    )
    .await;
    let id = b.clone();
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .write("operator", move |t| {
            t.execute(
                "UPDATE cards SET labels='[\"github\",\"hold\"]' WHERE id=?1",
                [id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    move_to(&td, &a, "review").await;
    assert!(met(&td, &b).await);
    assert_eq!(
        td.get(&format!("/api/cards/{b}")).await.ok()["column"],
        "backlog"
    );
    assert!(
        td.get("/api/mail?to=p/lead")
            .await
            .ok()
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["subject"].as_str().is_some_and(|s| s.contains("held")))
    );
    assert!(
        td.get(&format!("/api/cards/{c}")).await.ok()["comments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["body"] == "ready for work")
    );
}

#[tokio::test]
async fn wait_queries_use_dependency_indexes_and_limits_are_enforced() {
    let td = setup().await;
    let a = card(&td).await;
    let b = card(&td).await;
    let db = td.daemon.as_ref().unwrap().state.db.clone();
    let w = wait(&[(a.as_str(), None); 129], Mode::All, Action::default());
    let id = b.clone();
    assert!(
        db.write("operator", move |t| card_waits::set(
            t,
            &Actor::Operator,
            &id,
            &w
        ))
        .await
        .unwrap_err()
        .message
        .contains("128")
    );
    set(
        &td,
        &b,
        wait(&[(&a, Some("review"))], Mode::Any, Action::default()),
    )
    .await;
    db.read(move |c| {
        for (sql, parameter, expected) in [
            ("EXPLAIN QUERY PLAN SELECT card FROM card_deps WHERE depends_on=?1 AND card>'' ORDER BY card LIMIT 128", a, "card_deps_reverse"),
            ("EXPLAIN QUERY PLAN SELECT depends_on FROM card_dependency_unmet WHERE card=?1 LIMIT 129", b, "sqlite_autoindex_card_deps_1"),
        ] {
            let mut statement = c.prepare(sql)?;
            let details = statement.query_map([parameter], |r| r.get::<_, String>(3))?.collect::<Result<Vec<_>, _>>()?;
            assert!(details.iter().any(|s| s.contains(expected)), "{details:?}");
            assert!(!details.iter().any(|s| s.starts_with("SCAN d") || s.starts_with("SCAN a")), "{details:?}");
        }
        Ok(())
    }).await.unwrap();
}

#[tokio::test]
async fn blank_action_comments_are_refused_before_dependency_changes() {
    let td = setup().await;
    let a = card(&td).await;
    let b = card(&td).await;
    let db = td.daemon.as_ref().unwrap().state.db.clone();
    for comment in ["", " \n\t "] {
        let id = b.clone();
        let w = wait(
            &[(&a, Some("review"))],
            Mode::All,
            Action {
                comment: Some(comment.into()),
                ..Action::default()
            },
        );
        let error = db
            .write("operator", move |t| {
                card_waits::set(t, &Actor::Operator, &id, &w)
            })
            .await
            .unwrap_err();
        assert_eq!(error.code, terrariumd::error::Code::Invalid);
    }
    move_to(&td, &a, "review").await;
    assert_eq!(
        td.get(&format!("/api/cards/{a}")).await.ok()["column"],
        "review"
    );
}

#[tokio::test]
async fn no_lead_and_bad_actions_cannot_rollback_dependency_changes() {
    let td = TestDaemon::start().await;
    td.post("/api/projects", json!({"name":"p", "card_prefix":"N"}))
        .await
        .ok();
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
    td.delete(&format!("/api/cards/{a}")).await.ok();
    assert!(
        td.get(&format!("/api/cards/{a}")).await.ok()["archived"]
            .as_bool()
            .unwrap()
    );
    assert!(met(&td, &b).await);
    assert!(
        td.get("/api/mail?to=operator")
            .await
            .ok()
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["subject"]
                .as_str()
                .is_some_and(|s| s.contains("wait broken")))
    );

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
        td.get(&format!("/api/cards/{a}")).await.ok()["column"],
        "review"
    );
    assert_eq!(
        td.get(&format!("/api/cards/{b}")).await.ok()["column"],
        "backlog"
    );
    assert!(
        td.get("/api/mail?to=operator")
            .await
            .ok()
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["subject"]
                .as_str()
                .is_some_and(|s| s.contains("move refused")))
    );

    let c = card(&td).await;
    let d = card(&td).await;
    set(
        &td,
        &d,
        wait(
            &[(&c, Some("review"))],
            Mode::All,
            Action {
                comment: Some("valid".into()),
                ..Action::default()
            },
        ),
    )
    .await;
    let id = d.clone();
    let db = td.daemon.as_ref().unwrap().state.db.clone();
    // Model an invalid action persisted before blank comments were refused.
    db.write("operator", move |t| {
        t.execute(
            "UPDATE card_waits SET action=?2 WHERE card=?1",
            rusqlite::params![id, json!({"comment":" "}).to_string()],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    move_to(&td, &c, "review").await;
    assert!(met(&td, &d).await);
    assert_eq!(
        td.get(&format!("/api/cards/{c}")).await.ok()["column"],
        "review"
    );
    db.read(move |c| {
        let acted: bool = c.query_row("SELECT acted_at IS NOT NULL FROM card_waits WHERE card=?1", [&d], |r| r.get(0))?;
        let recorded: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM audit WHERE action='card.wait_action_failed' AND target=?1)", [&d], |r| r.get(0))?;
        assert!(acted && recorded);
        Ok(())
    }).await.unwrap();
}

#[tokio::test]
async fn shared_graph_budget_covers_all_roots_and_legacy_walks() {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    let td = setup().await;
    let template = card(&td).await;
    let waiter = card(&td).await;
    let legacy = card(&td).await;
    let roots: Vec<String> = (0..128).map(|n| format!("GRAPH-{n}")).collect();
    let w = Wait {
        dependencies: roots
            .iter()
            .map(|id| Dependency {
                card: id.clone(),
                target: None,
            })
            .collect(),
        mode: Mode::All,
        action: Action::default(),
        met_at: None,
    };
    let db = td.daemon.as_ref().unwrap().state.db.clone();
    db.write("operator", move |t| {
        // 128 roots share a 3900-card chain. Another 69 nodes test the global cap.
        for n in 0..4097 {
            t.execute("INSERT INTO cards(id,project,number,column_id,position,title,body,labels,created_by,created_at,updated_at)
                SELECT ?1,project,?2,column_id,?2,'graph','','[]',created_by,created_at,updated_at FROM cards WHERE id=?3",
                rusqlite::params![format!("GRAPH-{n}"), n+10000, template])?;
        }
        for n in 0..4027 {
            let next = if n < 128 { 128 } else { n+1 };
            t.execute("INSERT INTO card_deps(card,depends_on) VALUES (?1,?2)", rusqlite::params![format!("GRAPH-{n}"),format!("GRAPH-{next}")])?;
        }
        let steps = Arc::new(AtomicU64::new(0));
        let counter = steps.clone();
        t.progress_handler(1000, Some(move || {counter.fetch_add(1000, Ordering::Relaxed); false}));
        card_waits::set(t, &Actor::Operator, &waiter, &w)?;
        t.progress_handler(0, None::<fn() -> bool>);
        assert!(steps.load(Ordering::Relaxed) < 500_000, "shared set walk: {} VM steps", steps.load(Ordering::Relaxed));
        steps.store(0, Ordering::Relaxed);
        let counter = steps.clone();
        t.progress_handler(1000, Some(move || {counter.fetch_add(1000, Ordering::Relaxed); false}));
        let patch = serde_json::from_value(json!({"depends_on":roots}))?;
        terrariumd::store::board::update_card(t, &Actor::Operator, &legacy, patch)?;
        t.progress_handler(0, None::<fn() -> bool>);
        assert!(steps.load(Ordering::Relaxed) < 500_000, "shared legacy walk: {} VM steps", steps.load(Ordering::Relaxed));
        for n in 4027..4096 {
            t.execute("INSERT INTO card_deps(card,depends_on) VALUES (?1,?2)", rusqlite::params![format!("GRAPH-{n}"),format!("GRAPH-{}",n+1)])?;
        }
        assert!(card_waits::set(t, &Actor::Operator, &waiter, &w).unwrap_err().message.contains("4096"));
        // Oversized legacy adjacency lists are refused after at most 129 rows.
        for n in 1..130 {
            if n == 128 { continue }
            t.execute("INSERT INTO card_deps(card,depends_on) VALUES ('GRAPH-0',?1)", [format!("GRAPH-{n}")])?;
        }
        let patch = serde_json::from_value(json!({"depends_on":["GRAPH-0"]}))?;
        assert!(terrariumd::store::board::update_card(t, &Actor::Operator, &legacy, patch).unwrap_err().message.contains("128"));
        Ok(())
    }).await.unwrap();
}
