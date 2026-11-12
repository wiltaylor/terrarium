#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::{db, events::EventHub};
use rusqlite::params;
use terrarium_core::live_workspace::{
    AttachmentId, Caller, Enrollment, Holder, SourceIdentity, TrustDomainId, WorkspaceId,
};
use ulid::Ulid;

struct Call {
    bytes: Arc<[u8]>,
    reply: oneshot::Sender<ApiResult<(Envelope, Reply)>>,
}
impl Call {
    fn envelope(&self) -> Envelope {
        serde_json::from_slice(&self.bytes).unwrap()
    }
    fn respond(self, result: Reply) {
        let envelope = self.envelope();
        self.reply.send(Ok((envelope, result))).unwrap();
    }
}
struct Fake(mpsc::UnboundedSender<Call>);
impl Transport for Fake {
    fn exchange(&self, bytes: Arc<[u8]>) -> Exchange<'_> {
        Box::pin(async move {
            let (reply, wait) = oneshot::channel();
            self.0.send(Call { bytes, reply }).unwrap();
            wait.await.map_err(|_| fenced())?
        })
    }
}
fn fake() -> (Arc<dyn Transport>, mpsc::UnboundedReceiver<Call>) {
    let (send, receive) = mpsc::unbounded_channel();
    (Arc::new(Fake(send)), receive)
}

struct Fixture {
    _dir: tempfile::TempDir,
    dispatch: Dispatcher,
    grant: operations::Grant,
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let lock = Arc::new(crate::lock_state_dir(dir.path()).unwrap());
        let db = Db::new(
            db::open(&dir.path().join("terrarium.db")).unwrap(),
            EventHub::default(),
        );
        let grant = seed(&db).await;
        Self {
            _dir: dir,
            dispatch: Dispatcher::new(db, lock),
            grant,
        }
    }
    fn command(&self) -> Command {
        Command::Grant(Box::new(operations::Grant {
            provenance: self.grant.provenance.clone(),
            operation: self.grant.operation,
            registration: self.grant.registration,
            holder: self.grant.holder.clone(),
            owning_sandbox: self.grant.owning_sandbox.clone(),
        }))
    }
    fn submit(&mut self, transport: Arc<dyn Transport>) -> oneshot::Receiver<ApiResult<()>> {
        self.dispatch
            .submit(self.grant.registration, self.command(), transport)
            .unwrap()
    }
    async fn state(&self) -> (String, u64, u64, u64) {
        let id = self.grant.registration.0.to_string();
        self.dispatch.db.read(move |c| Ok(c.query_row(
            "SELECT state,allocated_revision,last_contiguous_revision,
             (SELECT count(*) FROM live_workspace_attachments WHERE registration_id = ?1 AND state = 'attached_verified')
             FROM live_workspace_targets WHERE registration_id = ?1", [id],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?)).await.unwrap()
    }
    async fn ended(&self) {
        let agent = self.grant.holder.enrollment.agent.to_string();
        self.dispatch
            .db
            .write_full("system", move |t| {
                t.execute(
                    "UPDATE agents SET status = 'stopped' WHERE id = ?1",
                    [agent],
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }
    async fn fenced(&self) {
        // A barrier on the same connection alone could precede the spawned
        // fence. Await its audit deterministically, without wall-clock sleeps.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if self.state().await.0 == "quarantined" {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            self.dispatch.entries[&self.grant.registration]
                .poison
                .load(Ordering::SeqCst)
        );
    }
}

async fn seed(db: &Db) -> operations::Grant {
    db.write_full("system", |t| {
        let registration = RegistrationId(Ulid::new());
        let host = Ulid::new();
        let target = Ulid::new().to_string();
        let domain = TrustDomainId(Ulid::new());
        let agent = terrarium_core::AgentId::new();
        let name = agent.to_string();
        let enrollment = Enrollment { workspace: WorkspaceId(Ulid::new()), host, agent,
            project: "fixture".into(), name: name.clone(), source: SourceIdentity { device: 1, inode: Ulid::new().0 as u64 } };
        t.execute("INSERT INTO live_workspace_targets (registration_id,host_id,target_id,trust_domain_id,runtime_id,container_id,namespace_dev,namespace_ino)
            VALUES (?1,?2,?3,?4,?5,?6,'1','2')", params![registration.0.to_string(),host.to_string(),target,domain.0.to_string(),Ulid::new().to_string(),"a".repeat(64)])?;
        t.execute("UPDATE live_workspace_targets SET state = 'reconcile_pending' WHERE registration_id = ?1", [registration.0.to_string()])?;
        t.execute("UPDATE live_workspace_targets SET state = 'verified' WHERE registration_id = ?1", [registration.0.to_string()])?;
        t.execute("INSERT INTO agents (id,name,project,harness,workspace,created_at,updated_at,status,policy,source)
            VALUES (?1,?2,'fixture','codex','/synthetic','now','now','idle',?3,'api')",
            params![agent.to_string(),name,serde_json::json!({"targets":[target]}).to_string()])?;
        t.execute("INSERT INTO live_workspaces VALUES (?1,?2,?3,'fixture',?4,'1',?5,'fresh')",
            params![enrollment.workspace.0.to_string(),host.to_string(),agent.to_string(),name,enrollment.source.inode.to_string()])?;
        Ok(operations::Grant { provenance: Provenance { daemon_instance: Ulid::new(), caller: Caller::LocalOperator },
            operation: OperationId(Ulid::new()), registration, holder: Holder { enrollment, attachment: AttachmentId(Ulid::new()), domain },
            owning_sandbox: "daemon-selected".into() })
    }).await.unwrap()
}

#[tokio::test]
async fn same_owner_serializes_allocation_through_runtime_and_caller_drop_is_not_cancellation() {
    let mut f = Fixture::new().await;
    let (transport, mut calls) = fake();
    let wait = f.submit(transport.clone());
    let call = calls.recv().await.unwrap();
    let original = call.envelope();
    drop(wait); // Allocated work belongs to the worker, not its HTTP waiter.
    let revoke = f
        .dispatch
        .submit(
            f.grant.registration,
            Command::Revoke {
                provenance: f.grant.provenance.clone(),
                operation: OperationId(Ulid::new()),
                attachment: f.grant.holder.attachment,
            },
            transport,
        )
        .unwrap();
    assert_eq!(f.state().await, ("verified".into(), 1, 0, 0));
    assert!(calls.try_recv().is_err());
    call.respond(Reply::Applied);
    let cleanup = calls.recv().await.unwrap();
    assert_eq!(cleanup.envelope().original.revision, 2);
    assert_eq!(
        cleanup.envelope().original.kind,
        terrarium_core::live_workspace::Kind::Revoke
    );
    cleanup.respond(Reply::PathRevokedVerified);
    revoke.await.unwrap().unwrap();
    assert_eq!(f.state().await, ("verified".into(), 2, 2, 0));
    let operation = original.original.operation;
    f.dispatch.db.write_full("system", move |t| {
        operations::commit_runtime(t, operation)?; // Exact double commit is harmless.
        let (bytes,hash,delivery): (Vec<u8>,Vec<u8>,String) = t.query_row(
            "SELECT original_request,original_hash,delivery FROM live_workspace_operations WHERE operation_id = ?1",
            [operation.0.to_string()], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        assert_eq!(bytes, serde_json::to_vec(&original.original).unwrap());
        assert_eq!(hash, original.original_hash);
        assert_eq!(delivery, "apply");
        Ok(())
    }).await.unwrap();
}

#[tokio::test]
async fn authority_ends_during_io_without_owner_or_permit_and_truthful_apply_precedes_revoke() {
    let mut f = Fixture::new().await;
    let (transport, mut calls) = fake();
    let wait = f.submit(transport);
    let call = calls.recv().await.unwrap();
    let frozen = call.bytes.clone();
    f.ended().await;
    assert_eq!(f.state().await.1, 1); // No concurrent allocate_revoke/delivery mutation.
    assert_eq!(call.bytes, frozen);
    call.respond(Reply::Applied);
    let cleanup = calls.recv().await.unwrap();
    assert_eq!(f.state().await.3, 0); // Renewed authority prevents publication.
    assert_eq!(cleanup.envelope().original.revision, 2);
    cleanup.respond(Reply::PathRevokedVerified);
    wait.await.unwrap().unwrap();
    assert_eq!(f.state().await, ("verified".into(), 2, 2, 0));
}

#[tokio::test]
async fn saturated_same_owner_queue_does_not_take_unrelated_transport_permits() {
    let mut f = Fixture::new().await;
    f.dispatch.permits = Arc::new(Semaphore::new(2));
    let (transport, mut calls) = fake();
    let wait = f.submit(transport.clone());
    let call = calls.recv().await.unwrap();
    let mut queued = Vec::new();
    for _ in 0..32 {
        queued.push(
            f.dispatch
                .submit(f.grant.registration, Command::Drain, transport.clone())
                .unwrap(),
        );
    }
    assert!(
        f.dispatch
            .submit(f.grant.registration, Command::Drain, transport.clone())
            .is_err()
    );
    let other = seed(&f.dispatch.db).await;
    let unrelated = f
        .dispatch
        .submit(
            other.registration,
            Command::Grant(Box::new(other)),
            transport,
        )
        .unwrap();
    let other_call = calls.recv().await.unwrap();
    assert_ne!(
        other_call.envelope().original.runtime.registration,
        f.grant.registration
    );
    other_call.respond(Reply::Applied);
    unrelated.await.unwrap().unwrap();
    drop(queued); // Queued dropped callers allocate nothing and release queue space.
    call.respond(Reply::Applied);
    wait.await.unwrap().unwrap();
    assert_eq!(f.state().await.1, 1);
}

#[tokio::test]
async fn dropped_queued_revoke_never_allocates() {
    let mut f = Fixture::new().await;
    let (transport, mut calls) = fake();
    let wait = f.submit(transport.clone());
    let call = calls.recv().await.unwrap();
    let queued = f
        .dispatch
        .submit(
            f.grant.registration,
            Command::Revoke {
                provenance: f.grant.provenance.clone(),
                operation: OperationId(Ulid::new()),
                attachment: f.grant.holder.attachment,
            },
            transport.clone(),
        )
        .unwrap();
    drop(queued);
    call.respond(Reply::Applied);
    wait.await.unwrap().unwrap();
    f.dispatch
        .submit(f.grant.registration, Command::Drain, transport)
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(f.state().await, ("verified".into(), 1, 1, 1));
}

#[tokio::test]
async fn wrong_receipt_fences_expected_owner_not_attacker_and_retains_originals() {
    let mut f = Fixture::new().await;
    let other = seed(&f.dispatch.db).await;
    let other_id = other.registration;
    let (transport, mut calls) = fake();
    let wait = f.submit(transport.clone());
    let call = calls.recv().await.unwrap();
    let original = call.envelope();
    let mut wrong = original.clone();
    wrong.original.operation = other.operation;
    wrong.original.runtime.registration = other_id;
    call.reply.send(Ok((wrong, Reply::Applied))).unwrap();
    assert!(wait.await.unwrap().is_err());
    f.fenced().await;
    assert!(
        f.dispatch
            .submit(f.grant.registration, Command::Drain, transport)
            .is_err()
    );
    f.dispatch
        .db
        .read(move |c| {
            let state: String = c.query_row(
                "SELECT state FROM live_workspace_targets WHERE registration_id = ?1",
                [other_id.0.to_string()],
                |r| r.get(0),
            )?;
            assert_eq!(state, "verified");
            let bytes: Vec<u8> = c.query_row(
                "SELECT original_request FROM live_workspace_operations WHERE operation_id = ?1",
                [original.original.operation.0.to_string()],
                |r| r.get(0),
            )?;
            assert_eq!(bytes, serde_json::to_vec(&original.original).unwrap());
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn unknown_send_and_failed_fence_write_keep_memory_poison_and_slot() {
    let mut f = Fixture::new().await;
    let (transport, mut calls) = fake();
    let wait = f.submit(transport.clone());
    let call = calls.recv().await.unwrap(); // Fake partial-send/remote commit, then lost receipt.
    f.dispatch.db.write_full("system", |t| {
        t.execute_batch("CREATE TRIGGER injected_fence_failure BEFORE UPDATE OF state ON live_workspace_targets WHEN NEW.state = 'quarantined' BEGIN SELECT RAISE(ABORT,'injected fence persistence failure'); END;")?;
        Ok(())
    }).await.unwrap();
    drop(call);
    assert!(wait.await.unwrap().is_err());
    let entry = &f.dispatch.entries[&f.grant.registration];
    // Wait for the worker's synchronous poison, not the best-effort DB fence.
    while !entry.task.is_finished() {
        tokio::task::yield_now().await;
    }
    assert!(entry.poison.load(Ordering::SeqCst));
    assert!(
        f.dispatch
            .submit(f.grant.registration, Command::Drain, transport)
            .is_err()
    );
    assert_eq!(f.dispatch.entries.len(), 1);
    assert_eq!(f.state().await.2, 0);
}

struct PanicTransport;
impl Transport for PanicTransport {
    fn exchange(&self, _: Arc<[u8]>) -> Exchange<'_> {
        Box::pin(async { panic!("injected exchange panic") })
    }
}

#[tokio::test]
async fn panic_and_worker_abort_fence_without_replacement() {
    let mut f = Fixture::new().await;
    let wait = f.submit(Arc::new(PanicTransport));
    assert!(wait.await.unwrap().is_err());
    f.fenced().await;
    let mut g = Fixture::new().await;
    let (transport, mut calls) = fake();
    let wait = g.submit(transport.clone());
    let _call = calls.recv().await.unwrap();
    g.dispatch.entries[&g.grant.registration].task.abort();
    assert!(wait.await.is_err());
    g.fenced().await;
    assert!(
        g.dispatch
            .submit(g.grant.registration, Command::Drain, transport)
            .is_err()
    );
}

#[tokio::test]
async fn timeout_is_unknown_not_permission_for_newer_revision() {
    let mut f = Fixture::new().await;
    let (transport, mut calls) = fake();
    let wait = f.submit(transport.clone());
    let _call = calls.recv().await.unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(11)).await;
    assert!(wait.await.unwrap().is_err());
    tokio::time::resume();
    f.fenced().await;
    assert!(
        f.dispatch
            .submit(f.grant.registration, Command::Drain, transport)
            .is_err()
    );
    assert_eq!(f.state().await.2, 0);
}

#[tokio::test]
async fn saved_helper_receipt_skips_transport_but_startup_quarantine_refuses_it() {
    let mut f = Fixture::new().await;
    let Command::Grant(grant) = f.command() else {
        unreachable!()
    };
    f.dispatch
        .db
        .write_full("system", move |t| {
            let envelope = operations::allocate_grant(t, &grant)?;
            operations::acknowledge(t, &envelope, Reply::Applied)?;
            Ok(())
        })
        .await
        .unwrap();
    let (transport, mut calls) = fake();
    f.dispatch
        .submit(f.grant.registration, Command::Drain, transport)
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    assert!(calls.try_recv().is_err());
    assert_eq!(f.state().await.3, 1);
    let mut g = Fixture::new().await;
    let Command::Grant(grant) = g.command() else {
        unreachable!()
    };
    g.dispatch
        .db
        .write_full("system", move |t| {
            let envelope = operations::allocate_grant(t, &grant)?;
            operations::acknowledge(t, &envelope, Reply::Applied)?;
            crate::store::live_workspaces::quarantine_on_restart(t)?;
            Ok(())
        })
        .await
        .unwrap();
    let (transport, mut calls) = fake();
    assert!(
        g.dispatch
            .submit(g.grant.registration, Command::Drain, transport)
            .unwrap()
            .await
            .unwrap()
            .is_err()
    );
    assert!(calls.try_recv().is_err());
    assert_eq!(g.state().await.3, 0);
}

#[tokio::test]
async fn authority_loss_before_selection_sends_original_superseded_barrier_then_revoke() {
    let mut f = Fixture::new().await;
    let Command::Grant(grant) = f.command() else {
        unreachable!()
    };
    let original = f
        .dispatch
        .db
        .write_full("system", move |t| operations::allocate_grant(t, &grant))
        .await
        .unwrap();
    f.ended().await;
    let (transport, mut calls) = fake();
    let wait = f
        .dispatch
        .submit(f.grant.registration, Command::Drain, transport)
        .unwrap();
    let barrier = calls.recv().await.unwrap();
    assert_eq!(barrier.envelope().original, original.original);
    assert_eq!(barrier.envelope().original_hash, original.original_hash);
    assert_eq!(
        barrier.envelope().delivery,
        terrarium_core::live_workspace::Delivery::Superseded
    );
    barrier.respond(Reply::Superseded);
    let revoke = calls.recv().await.unwrap();
    assert_eq!(revoke.envelope().original.revision, 2);
    revoke.respond(Reply::PathRevokedVerified);
    wait.await.unwrap().unwrap();
    assert_eq!(f.state().await.3, 0);
}

#[tokio::test]
async fn receipt_and_runtime_persistence_failures_fence_and_preserve_truth() {
    for boundary in ["helper_committed", "daemon_committed"] {
        let mut f = Fixture::new().await;
        let (transport, mut calls) = fake();
        let wait = f.submit(transport);
        let call = calls.recv().await.unwrap();
        let sql = format!(
            "CREATE TRIGGER injected_failure BEFORE UPDATE OF phase ON live_workspace_operations WHEN NEW.phase = '{boundary}' BEGIN SELECT RAISE(ABORT,'injected commit failure'); END;"
        );
        f.dispatch
            .db
            .write_full("system", move |t| {
                t.execute_batch(&sql)?;
                Ok(())
            })
            .await
            .unwrap();
        call.respond(Reply::Applied);
        assert!(wait.await.unwrap().is_err());
        f.fenced().await;
        let expected = boundary == "daemon_committed";
        f.dispatch
            .db
            .read(move |c| {
                let saved: bool = c.query_row(
                    "SELECT result IS NOT NULL FROM live_workspace_operations",
                    [],
                    |r| r.get(0),
                )?;
                assert_eq!(saved, expected);
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(f.state().await.2, 0);
    }
}

#[tokio::test]
async fn authority_loss_after_receipt_before_runtime_never_attaches() {
    let mut f = Fixture::new().await;
    let (transport, mut calls) = fake();
    let wait = f.submit(transport);
    let call = calls.recv().await.unwrap();
    // End the independently stored policy fact at the precise durable receipt
    // boundary. This does not change the selected operation's delivery.
    f.dispatch.db.write_full("system", |t| {
        t.execute_batch("CREATE TRIGGER end_authority AFTER UPDATE OF phase ON live_workspace_operations WHEN NEW.phase = 'helper_committed' AND NEW.kind = 'grant' BEGIN UPDATE agents SET policy = '{}'; END;")?;
        Ok(())
    }).await.unwrap();
    call.respond(Reply::Applied);
    let cleanup = calls.recv().await.unwrap();
    assert_eq!(f.state().await.3, 0);
    cleanup.respond(Reply::PathRevokedVerified);
    wait.await.unwrap().unwrap();
    assert_eq!(f.state().await.3, 0);
}

#[tokio::test]
async fn queue_deadline_precedes_allocation_and_expired_work_does_not_poison_owner() {
    let mut f = Fixture::new().await;
    let (transport, mut calls) = fake();
    let wait = f.submit(transport.clone());
    let call = calls.recv().await.unwrap();
    let expired = f
        .dispatch
        .submit(
            f.grant.registration,
            Command::Revoke {
                provenance: f.grant.provenance.clone(),
                operation: OperationId(Ulid::new()),
                attachment: f.grant.holder.attachment,
            },
            transport.clone(),
        )
        .unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(6)).await;
    call.respond(Reply::Applied);
    tokio::time::resume();
    wait.await.unwrap().unwrap();
    assert!(expired.await.unwrap().is_err());
    f.dispatch
        .submit(f.grant.registration, Command::Drain, transport)
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(f.state().await, ("verified".into(), 1, 1, 1));
}

fn config(root: &std::path::Path, socket: &str) -> crate::DaemonConfig {
    crate::DaemonConfig {
        socket: root.join(socket),
        listen: None,
        token: None,
        state_dir: root.join("state"),
        config_dir: root.join("config"),
        work_dir: root.join("work"),
        repos_dir: root.join("repos"),
        max_targets_per_project: crate::settings::DEFAULT_MAX_TARGETS_PER_PROJECT,
        supervisor: Arc::new(crate::supervisor::InMemorySupervisor::new(
            Duration::from_millis(1),
        )),
        insecure_agent_header: true,
        artifact_listen: None,
        web: Default::default(),
    }
}

#[tokio::test]
async fn actual_daemon_startup_refuses_second_owner_and_worker_fence_outlives_shutdown() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("config")).unwrap();
    let mut daemon = crate::start(config(root.path(), "first.sock"))
        .await
        .unwrap();
    let grant = seed(&daemon.state.db).await;
    let registration = grant.registration;
    let second = crate::start(config(root.path(), "second.sock")).await;
    assert!(second.err().unwrap().to_string().contains("another daemon"));
    assert!(!root.path().join("second.sock").exists());
    let (transport, mut calls) = fake();
    let wait = daemon
        .workspace_dispatch
        .submit(registration, Command::Grant(Box::new(grant)), transport)
        .unwrap();
    let _unknown = calls.recv().await.unwrap();
    let lock = Arc::downgrade(&daemon._state_lock);
    daemon.shutdown(); // Aborts but cannot synchronously destroy an async worker.
    let second = crate::start(config(root.path(), "second.sock")).await;
    assert!(second.err().unwrap().to_string().contains("another daemon"));
    assert!(wait.await.is_err());
    tokio::time::timeout(Duration::from_secs(10), async {
        while lock.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut restarted = crate::start(config(root.path(), "third.sock"))
        .await
        .unwrap();
    let (transport, mut calls) = fake();
    let wait = restarted
        .workspace_dispatch
        .submit(registration, Command::Drain, transport)
        .unwrap();
    assert!(wait.await.unwrap().is_err());
    assert!(calls.try_recv().is_err());
    restarted.shutdown_and_wait().await;
    // The waiting shutdown variant drains both workers and their fence writes
    // before returning, so immediate ownership transfer needs no retry loop.
    let mut daemon = crate::start(config(root.path(), "fourth.sock")).await.unwrap();
    let grant = seed(&daemon.state.db).await;
    let (transport, mut calls) = fake();
    let wait = daemon.workspace_dispatch.submit(grant.registration, Command::Grant(Box::new(grant)), transport).unwrap();
    let _unknown = calls.recv().await.unwrap();
    daemon.shutdown_and_wait().await;
    assert!(wait.await.is_err());
    crate::start(config(root.path(), "fifth.sock")).await.unwrap().shutdown_and_wait().await;
}

#[tokio::test]
async fn transport_permit_bound_is_shared_only_by_runnable_owners() {
    let mut f = Fixture::new().await;
    f.dispatch.permits = Arc::new(Semaphore::new(2));
    let (transport, mut calls) = fake();
    let first = f.submit(transport.clone());
    let first_call = calls.recv().await.unwrap();
    let second = seed(&f.dispatch.db).await;
    let second = f.dispatch.submit(second.registration, Command::Grant(Box::new(second)), transport.clone()).unwrap();
    let second_call = calls.recv().await.unwrap();
    let third = seed(&f.dispatch.db).await;
    let third_id = third.registration.0.to_string();
    let third = f.dispatch.submit(third.registration, Command::Grant(Box::new(third)), transport).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let id = third_id.clone();
            let allocated: u64 = f.dispatch.db.read(move |c| Ok(c.query_row("SELECT allocated_revision FROM live_workspace_targets WHERE registration_id = ?1", [id], |r| r.get(0))?)).await.unwrap();
            if allocated == 1 { break; }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    assert!(calls.try_recv().is_err());
    assert_eq!(f.dispatch.permits.available_permits(), 0);
    first_call.respond(Reply::Applied);
    let third_call = calls.recv().await.unwrap();
    third_call.respond(Reply::Applied);
    second_call.respond(Reply::Applied);
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    third.await.unwrap().unwrap();
    assert_eq!(f.dispatch.permits.available_permits(), 2);
}
