#![allow(clippy::unwrap_used, clippy::expect_used)]

use rusqlite::Connection;
use std::collections::BTreeMap;
use terrarium_core::resources::Amounts;
use terrariumd::db::{self, Db};
use terrariumd::events::EventHub;
use terrariumd::store::{
    hosts,
    reservations::{self as r, Holder, Request, State},
};

async fn database() -> Db {
    let mut conn = Connection::open_in_memory().unwrap();
    db::migrate(&mut conn).unwrap();
    let db = Db::new(conn, EventHub::default());
    db.write("operator", |t| {
        hosts::create(t, "host", false)?;
        t.execute(
            "UPDATE hosts SET capabilities=?1 WHERE name='host'",
            [r#"{"capacity":{"cpu_ids":[0,1,2,3],"memory_bytes":4096}}"#],
        )?;
        r::configure(t, "host", 50, BTreeMap::new())
    })
    .await
    .unwrap();
    db
}

fn request(holder: &str, cpu: u64, queue: bool) -> Request {
    Request {
        gpus: vec![],
        host: "host".into(),
        project: "p".into(),
        holder: Holder::Target(holder.into()),
        target: holder.into(),
        amounts: Amounts {
            cpu_millis: cpu,
            ..Default::default()
        },
        exclusive: false,
        reason: "test".into(),
        duration_ms: 1_200_000,
        queue_ms: queue.then_some(2_000_000),
    }
}

#[tokio::test]
async fn gpu_exclusive_shared_fifo_and_cleanup_accounting() {
    let db = database().await;
    db.write("operator", |t| {
        t.execute("UPDATE hosts SET capabilities=json_set(capabilities,'$.gpus',json(?1)) WHERE name='host'", [r#"[{"uuid":"GPU-abcd","name":"test","memory_bytes":8192,"used_bytes":0}]"#])?;
        let gpu = |holder: &str, exclusive: bool, vram: u64, queue: bool| {
            let mut req = request(holder, 0, queue);
            req.gpus = vec![terrarium_core::resources::GpuClaim { uuid: "GPU-abcd".into(), exclusive, vram_bytes: vram }];
            req
        };
        assert!(r::request(t, gpu("bad", false, 0, false), 0).is_err());
        assert!(r::request(t, gpu("big", false, 8193, true), 0).is_err());
        let a = r::request(t, gpu("a", false, 4096, false), 0)?;
        let b = r::request(t, gpu("b", false, 4096, false), 0)?;
        assert!(r::request(t, gpu("c", false, 1, false), 0).is_err());
        assert!(r::request(t, gpu("c", true, 0, false), 0).is_err());
        let queued = r::request(t, gpu("c", true, 0, true), 0)?;
        assert_eq!(queued.state, State::Queued);
        for id in [&a.id, &b.id] { r::end(t, id, false)?; }
        assert!(r::advance(t, "host", 1)?.is_none());
        for id in [&a.id, &b.id] { r::finish_end(t, id)?; }
        assert_eq!(r::advance(t, "host", 2)?.unwrap().id, queued.id);
        assert!(r::request(t, gpu("d", false, 1, false), 2).is_err());
        Ok(())
    }).await.unwrap();
}

#[tokio::test]
async fn identities_stay_until_reservation_cleanup_finishes() {
    let db = database().await;
    db.write("operator", |t| {
        let mut req = request("box", 1000, false);
        req.holder = Holder::Agent("worker".into());
        let reservation = r::request(t, req, 0)?;
        for kind_id in [("target", "box"), ("agent", "worker")] {
            assert!(r::guard_identity(t, kind_id.0, kind_id.1).is_err());
        }
        r::guard_identity(t, "target", "other")?;
        r::activate(t, &reservation.id)?;
        r::end(t, &reservation.id, false)?;
        assert!(r::guard_identity(t, "target", "box").is_err());
        r::finish_end(t, &reservation.id)?;
        r::guard_identity(t, "target", "box")?;
        r::guard_identity(t, "agent", "worker")?;
        r::request(t, request("full", 2000, false), 0)?;
        let queued = r::request(t, request("queued", 1000, true), 0)?;
        assert!(r::guard_identity(t, "target", "queued").is_err());
        r::cancel(t, &queued.id)?;
        r::guard_identity(t, "target", "queued")
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn concurrent_admission_never_overcommits() {
    let db = database().await;
    let (a, b) = tokio::join!(
        db.write("operator", |t| r::request(t, request("a", 2000, false), 0)),
        db.write("operator", |t| r::request(t, request("b", 2000, false), 0))
    );
    assert_ne!(a.is_ok(), b.is_ok());
    let error = a.err().or(b.err()).unwrap();
    assert_eq!(error.code, terrariumd::error::Code::Conflict);
    assert_eq!(
        error.details.unwrap()["holders"].as_array().unwrap().len(),
        1
    );
}

#[tokio::test]
async fn fifo_does_not_backfill_and_cleanup_releases_capacity() {
    let db = database().await;
    db.write("operator", |t| {
        let a = r::request(t, request("a", 1000, false), 0)?;
        r::activate(t, &a.id)?;
        let b = r::request(t, request("b", 2000, true), 0)?;
        let c = r::request(t, request("c", 1000, true), 0)?;
        assert_eq!(b.state, State::Queued);
        assert_eq!(c.state, State::Queued);
        assert!(r::advance(t, "host", 1)?.is_none());
        r::end(t, &a.id, true)?;
        assert!(
            r::advance(t, "host", 2)?.is_none(),
            "ending still occupies capacity"
        );
        r::finish_end(t, &a.id)?;
        let next = r::advance(t, "host", 3)?.unwrap();
        assert_eq!(next.id, b.id);
        assert_eq!(next.until, Some(1_200_003));
        assert!(r::advance(t, "host", 4)?.is_none());
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn queue_cancel_timeout_and_impossible_request() {
    let db = database().await;
    db.write("operator", |t| {
        assert!(r::request(t, request("too-big", 3000, true), 0).is_err());
        r::request(t, request("a", 2000, false), 0)?;
        let b = r::request(t, request("b", 1000, true), 0)?;
        assert_eq!(r::cancel(t, &b.id)?.state, State::Cancelled);
        let c = r::request(t, request("c", 1000, true), 0)?;
        assert!(r::advance(t, "host", 2_000_000)?.is_none());
        assert_eq!(r::get(t, &c.id)?.state, State::TimedOut);
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn warnings_once_per_threshold_and_extension_rearms() {
    let db = database().await;
    db.write("operator", |t| {
        let a = r::request(t, request("a", 1000, false), 0)?;
        r::activate(t, &a.id)?;
        assert!(r::warnings(t, "host", 299_999)?.is_empty());
        assert_eq!(r::warnings(t, "host", 300_000)?[0].1, 15);
        assert!(r::warnings(t, "host", 300_001)?.is_empty());
        assert_eq!(r::warnings(t, "host", 900_000)?[0].1, 5);
        assert!(r::warnings(t, "host", 900_001)?.is_empty());
        r::extend(t, &a.id, 2_400_000, 900_001)?;
        assert_eq!(r::warnings(t, "host", 1_500_000)?[0].1, 15);
        assert!(r::warnings(t, "host", 2_400_000)?.is_empty());
        assert!(r::extend(t, &a.id, 3_600_000, 2_400_000).is_err());
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn exclusive_allocations_are_disjoint_and_whole_cores() {
    let db = database().await;
    db.write("operator", |t| {
        let mut req = request("a", 500, false);
        req.exclusive = true;
        assert!(r::request(t, req, 0).is_err());
        let mut req = request("a", 1000, false);
        req.exclusive = true;
        let a = r::request(t, req, 0)?;
        let mut req = request("b", 1000, false);
        req.exclusive = true;
        let b = r::request(t, req, 0)?;
        assert_eq!(a.cores.len(), 1);
        assert_eq!(b.cores.len(), 1);
        assert_ne!(a.cores, b.cores);
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn changes_write_audit_and_events() {
    let db = database().await;
    db.write("operator", |t| {
        let a = r::request(t, request("a", 1000, false), 0)?;
        r::activate(t, &a.id)?;
        r::end(t, &a.id, false)?;
        r::finish_end(t, &a.id)?;
        let audits: i64 =
            t.query_row("SELECT count(*) FROM audit WHERE target=?1", [&a.id], |r| {
                r.get(0)
            })?;
        let events: i64 = t.query_row(
            "SELECT count(*) FROM events WHERE kind='reservation'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(audits, 4);
        assert_eq!(events, 4);
        assert_eq!(r::get(t, &a.id)?.state, State::Retracted);
        Ok(())
    })
    .await
    .unwrap();
}
