//! Reporting fakes only. No target commands, real processes or signals.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common::TestDaemon;
use serde_json::json;
use terrariumd::store::{run_cleanup as cleanup, waits::{self, NewWait, Wait, WaitKind, WaitState}};

async fn setup() -> (TestDaemon, Wait) {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let agent = td.agent("p", "owner", json!({})).await["id"].as_str().unwrap().to_owned();
    let db = td.daemon.as_ref().unwrap().state.db.clone();
    let w = db.write("operator", move |t| waits::create(t, NewWait {
        agent, kind: WaitKind::Process, spec: json!({"session":"fake-session", "target":"fake-target"}),
        summary:"fake run".into(), target_agent:None, reason:None, timeout_at:None, due_at:None,
    })).await.unwrap();
    (td, w)
}

#[tokio::test]
async fn missing_route_and_concurrent_retries_keep_one_operation() {
    let (td, w) = setup().await;
    let path = format!("/api/waits/{}?kill=true",w.id);
    let (a,b) = tokio::join!(td.delete(&path), td.delete(&path));
    let a = a.ok(); let b = b.ok();
    assert_eq!(a["cleanup"]["operation_id"],b["cleanup"]["operation_id"]);
    assert!(a["cleanup"]["operation_id"].is_string());
    let final_view = td.get(&format!("/api/waits/{}", w.id)).await;
    assert_eq!(final_view.ok()["state"],"cancelled");
    assert_eq!(final_view.ok()["cleanup"]["status"],"unavailable");
    let retry = td.delete(&path).await;
    assert_eq!(retry.ok()["cleanup"],final_view.ok()["cleanup"]);
}

#[tokio::test]
async fn legacy_cancel_and_cas_loss_never_create_intent() {
    let (td,w) = setup().await;
    td.delete(&format!("/api/waits/{}",w.id)).await.ok();
    let retry = td.delete(&format!("/api/waits/{}?kill=true",w.id)).await;
    assert_eq!(retry.ok()["cleanup"]["reason"],"no_recorded_intent");
    let db = td.daemon.as_ref().unwrap().state.db.clone();
    db.write("operator", move |t| {
        let (op, created) = cleanup::cancel(t,&w,"operator",false)?;
        assert!(op.is_none()); assert!(!created);
        Ok(())
    }).await.unwrap();
}

#[tokio::test]
async fn phase_cas_restart_no_replay_and_stale_completion() {
    for dispatch in [false,true] {
        let (td,w) = setup().await;
        let db = td.daemon.as_ref().unwrap().state.db.clone();
        db.write("operator", move |t| {
            let (op,created) = cleanup::cancel(t,&w,"operator",false)?;
            assert!(created);
            let id = op.unwrap().operation_id;
            let (duplicate,created) = cleanup::cancel(t,&w,"operator",false)?;
            assert!(!created); assert_eq!(duplicate.unwrap().operation_id,id);
            if dispatch {
                assert!(cleanup::dispatch(t,&id)?);
                assert!(!cleanup::dispatch(t,&id)?);
            }
            cleanup::recover(t)?;
            cleanup::recover(t)?;
            assert!(!cleanup::dispatch(t,&id)?);
            cleanup::finish(t,&id,dispatch,"stale_result",None)?;
            let view = serde_json::to_value(cleanup::get(t,&w)?.unwrap()).unwrap();
            assert_eq!(view["status"],if dispatch {"unknown"} else {"unavailable"});
            assert_eq!(view["reason"],"daemon_restart_no_replay");
            assert_eq!(waits::get(t,&w.id)?.state,WaitState::Cancelled);
            Ok(())
        }).await.unwrap();
    }
}

#[tokio::test]
async fn intent_insert_failure_rolls_back_wait_cancellation() {
    let (td,w) = setup().await;
    let db = td.daemon.as_ref().unwrap().state.db.clone();
    db.write("operator", |t| { t.execute_batch("CREATE TRIGGER refuse_cleanup BEFORE INSERT ON run_cleanup BEGIN SELECT RAISE(ABORT,'injected persistence failure'); END;")?; Ok(()) }).await.unwrap();
    let snapshot = w.clone();
    assert!(db.write("operator",move |t| cleanup::cancel(t,&snapshot,"operator",false)).await.is_err());
    let state = db.read(move |c| Ok(waits::get(c,&w.id)?.state)).await.unwrap();
    assert_eq!(state,WaitState::Open);
}

#[tokio::test]
async fn snapshots_cannot_be_rebound() {
    let (td,w) = setup().await;
    let db = td.daemon.as_ref().unwrap().state.db.clone();
    db.write("operator", move |t| {
        cleanup::cancel(t,&w,"operator",false)?;
        for field in ["project","owner","session","target"] {
            let mut changed = w.clone();
            match field {
                "project" => changed.project = "foreign".into(),
                "owner" => changed.agent = "foreign".into(),
                "session" => changed.spec["session"] = json!("foreign"),
                _ => changed.spec["target"] = json!("foreign"),
            }
            assert!(cleanup::get(t,&changed).is_err());
            assert!(cleanup::cancel(t,&changed,"operator",false).is_err());
        }
        Ok(())
    }).await.unwrap();
}

#[tokio::test]
async fn kill_checks_owner_project_admin_and_nonprocess_before_mutation() {
    let (td,w) = setup().await;
    let other = td.agent("p","other",json!({})).await["id"].as_str().unwrap().to_owned();
    td.project("foreign").await;
    let foreign = td.agent("foreign","admin",json!({"roles":["project_admin"]})).await["id"].as_str().unwrap().to_owned();
    let path = format!("/api/waits/{}?kill=true",w.id);
    for actor in [&other,&foreign] {
        let denied = td.delete_as(actor,&path).await;
        assert_ne!(denied.status,hyper::StatusCode::OK);
    }
    assert_eq!(td.get(&format!("/api/waits/{}",w.id)).await.ok()["state"],"open");
    let admin = td.agent("p","admin",json!({"roles":["project_admin"]})).await["id"].as_str().unwrap().to_owned();
    assert_eq!(td.delete_as(&admin,&path).await.ok()["cleanup"]["status"],"unavailable");
    let other_wait = td.post(&format!("/api/agents/{}/waits",w.agent),json!({"kind":"time","for":"1h"})).await;
    let id = other_wait.ok()["id"].as_str().unwrap();
    assert_eq!(td.delete(&format!("/api/waits/{id}?kill=true")).await.status,hyper::StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(td.get(&format!("/api/waits/{id}")).await.ok()["state"],"open");
}

#[tokio::test]
async fn receipt_write_failure_leaves_unknown_and_retry_cannot_dispatch() {
    let (td,w) = setup().await;
    let db = td.daemon.as_ref().unwrap().state.db.clone();
    let snapshot = w.clone();
    let id = db.write_full("operator", move |t| {
        let (op,_) = cleanup::cancel(t,&snapshot,"operator",false)?;
        let id = op.unwrap().operation_id;
        assert!(cleanup::dispatch(t,&id)?);
        t.execute_batch("CREATE TRIGGER refuse_receipt BEFORE UPDATE ON run_cleanup WHEN NEW.phase='finished' BEGIN SELECT RAISE(ABORT,'injected receipt failure'); END;")?;
        Ok(id)
    }).await.unwrap();
    let op = id.clone();
    assert!(db.write_full("operator", move |t| cleanup::finish(t,&op,true,"legacy_report_not_identity_bound",None)).await.is_err());
    let snapshot = w.clone();
    db.write_full("operator", move |t| {
        let (op,created) = cleanup::cancel(t,&snapshot,"operator",false)?;
        assert!(!created);
        assert_eq!(op.unwrap().operation_id,id);
        assert!(!cleanup::dispatch(t,&id)?);
        Ok(())
    }).await.unwrap();
    let view = td.get(&format!("/api/waits/{}",w.id)).await;
    assert_eq!(view.ok()["cleanup"]["status"],"unknown");
    assert_eq!(view.ok()["cleanup"]["reason"],"dispatch_started_effect_unknown");
}

#[tokio::test]
async fn resolved_cas_loss_has_no_cleanup_intent() {
    let (td,w) = setup().await;
    let db = td.daemon.as_ref().unwrap().state.db.clone();
    let snapshot = w.clone();
    db.write("operator", move |t| {
        waits::finish(t,&snapshot.id,WaitState::Resolved,json!({"exit_code":0}),false,0)?;
        let (op,created) = cleanup::cancel(t,&snapshot,"operator",false)?;
        assert!(!created); assert!(op.is_none());
        Ok(())
    }).await.unwrap();
    let view = td.delete(&format!("/api/waits/{}?kill=true",w.id)).await;
    assert_eq!(view.ok()["state"],"resolved");
    assert_eq!(view.ok()["cleanup"]["reason"],"no_recorded_intent");
}
