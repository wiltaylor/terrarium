//! Ordered bootstrap execution. No production backend or service route is
//! selected here. Incomplete recovery is deliberately quarantine-only.

use super::Store;
use terrarium_core::live_workspace::bootstrap::{Journal, Request, Scaffold, Step};
use terrarium_core::live_workspace::{Reply, RuntimeIdentity};

/// Implementations must retain pinned scaffold/runtime FDs for the entire
/// operation. Paths and Docker options are helper-selected, never request data.
pub trait Backend {
    /// Recompute from protected configuration and current runtime profile.
    /// Required for retries too, including a previously committed result.
    fn check_configuration(&mut self, request: &Request) -> Result<(), Reply>;
    /// Match the protected specification digest and measured runtime profile,
    /// then refuse existing children/runtimes, including apparently empty ones.
    /// This check is not ownership proof and must precede the initial intent.
    fn check_absent(&mut self, request: &Request) -> Result<(), Reply>;
    fn create_scaffold(&mut self, request: &Request, store: &Store) -> Result<Scaffold, Reply>;
    /// Create only, without starting. Return a verified full container ID.
    fn create_receiver(&mut self, request: &Request, scaffold: &Scaffold) -> Result<String, Reply>;
    /// Recheck scaffold identity, start the exact bound ID, and pin and verify
    /// the complete runtime configuration and mount namespace.
    fn start_and_verify(
        &mut self,
        request: &Request,
        scaffold: &Scaffold,
        container_id: &str,
    ) -> Result<RuntimeIdentity, Reply>;
    /// Fresh evidence and the normal runtime journal must be durable before
    /// this returns. It must be safe to quarantine after an interrupted call.
    fn commit_runtime(&mut self, request: &Request, runtime: &RuntimeIdentity)
    -> Result<(), Reply>;
}

/// Owns the exclusive store lock throughout all external actions and syncs.
pub struct Bootstrap<B> {
    store: Store,
    backend: B,
    fenced: bool,
    prepared: Option<Request>,
}

impl<B: Backend> Bootstrap<B> {
    pub fn new(store: Store, backend: B) -> Self {
        Self {
            store,
            backend,
            fenced: false,
            prepared: None,
        }
    }

    /// Mint a one-use continuation only for freshly allocated metadata. An
    /// existing journal directory can never mint this token after a crash.
    pub fn allocate(
        roots: &super::registration_roots::Roots,
        request: Request,
        mut backend: B,
    ) -> Result<Self, Reply> {
        Journal::new(request.clone())?;
        backend.check_configuration(&request)?;
        backend.check_absent(&request)?;
        let store = roots.begin(request.clone())?;
        Ok(Self {
            store,
            backend,
            fenced: false,
            prepared: Some(request),
        })
    }

    pub fn create_prepared(&mut self) -> Result<(), Reply> {
        if self.fenced {
            return Err(Reply::Quarantined);
        }
        let request = self.prepared.take().ok_or(Reply::Conflict)?;
        self.fenced = true;
        let journal = self.store.load_bootstrap()?;
        if !journal.initial() || journal.original() != &request {
            return Err(Reply::Quarantined);
        }
        self.execute(request, journal)
    }

    /// No automatic recovery adoption is implemented. A committed duplicate
    /// returns only its stored identity, never invokes the backend again.
    pub fn retry(&mut self, request: &Request) -> Result<(), Reply> {
        if self.fenced {
            return Err(Reply::Quarantined);
        }
        let mut journal = self.store.load_bootstrap()?;
        if journal.original() != request {
            return Err(Reply::Conflict);
        }
        self.check_configuration(&mut journal)?;
        journal.recover_without_evidence();
        self.store.save_bootstrap(&journal)?;
        journal.retry(request)
    }

    fn advance(&mut self, journal: &mut Journal, step: Step) -> Result<(), Reply> {
        journal.advance(step)?;
        self.store.save_bootstrap(journal)
    }

    fn check_configuration(&mut self, journal: &mut Journal) -> Result<(), Reply> {
        if self
            .backend
            .check_configuration(journal.original())
            .is_err()
        {
            self.fenced = true;
            journal.quarantine();
            self.store
                .save_bootstrap(journal)
                .map_err(|_| Reply::Quarantined)?;
            return Err(Reply::Quarantined);
        }
        Ok(())
    }

    /// The caller has already authenticated the peer and admitted authority.
    /// Every error after intent fences this instance, even a failed sync. No
    /// cleanup deletes foreign objects, and no external action is retried.
    pub fn create(&mut self, request: Request) -> Result<(), Reply> {
        if self.fenced {
            return Err(Reply::Quarantined);
        }
        if self.prepared.is_some() {
            return Err(Reply::Conflict);
        }
        let journal = Journal::new(request.clone())?;
        self.backend.check_configuration(&request)?;
        self.backend.check_absent(&request)?;
        self.fenced = true;
        self.store.begin_bootstrap(&journal)?;
        self.execute(request, journal)
    }

    fn execute(&mut self, request: Request, mut journal: Journal) -> Result<(), Reply> {
        self.check_configuration(&mut journal)?;
        let result = (|| {
            let scaffold = self.backend.create_scaffold(&request, &self.store)?;
            self.advance(&mut journal, Step::ScaffoldVerified(scaffold.clone()))?;
            self.advance(&mut journal, Step::CreatePending)?;
            let container = self.backend.create_receiver(&request, &scaffold)?;
            self.advance(&mut journal, Step::ContainerBound(container.clone()))?;
            let runtime = self
                .backend
                .start_and_verify(&request, &scaffold, &container)?;
            self.advance(&mut journal, Step::ReceiverVerified(runtime.clone()))?;
            self.backend.commit_runtime(&request, &runtime)?;
            self.advance(&mut journal, Step::Committed)
        })();
        if result.is_err() {
            self.fenced = true;
            journal.quarantine();
            // Failure to persist quarantine never permits another action in
            // this instance. An incomplete durable journal fences on restart.
            let _ = self.store.save_bootstrap(&journal);
            return Err(Reply::Quarantined);
        }
        self.fenced = false;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use terrarium_core::live_workspace::{NamespaceIdentity, SourceIdentity};

    struct Fake {
        calls: usize,
        fail: usize,
        configuration_changed: bool,
        post_effect: bool,
        effects: usize,
    }

    impl Fake {
        fn fresh(fail: usize) -> Self {
            Self {
                calls: 0,
                fail,
                configuration_changed: false,
                post_effect: false,
                effects: 0,
            }
        }
        fn action(&mut self) -> Result<(), Reply> {
            self.calls += 1;
            if self.post_effect || self.calls != self.fail {
                self.effects += 1;
            }
            if self.calls == self.fail {
                Err(Reply::Failed)
            } else {
                Ok(())
            }
        }
    }

    impl Backend for Fake {
        fn check_configuration(&mut self, r: &Request) -> Result<(), Reply> {
            if self.configuration_changed || r.configuration != [1; 32] {
                Err(Reply::Conflict)
            } else {
                Ok(())
            }
        }
        fn check_absent(&mut self, _: &Request) -> Result<(), Reply> {
            Ok(())
        }
        fn create_scaffold(&mut self, _: &Request, _: &Store) -> Result<Scaffold, Reply> {
            self.action()?;
            Ok(Scaffold {
                publisher: SourceIdentity {
                    device: 1,
                    inode: 1,
                },
                staging: SourceIdentity {
                    device: 1,
                    inode: 2,
                },
                state: SourceIdentity {
                    device: 1,
                    inode: 3,
                },
            })
        }
        fn create_receiver(&mut self, _: &Request, _: &Scaffold) -> Result<String, Reply> {
            self.action()?;
            Ok("a".repeat(64))
        }
        fn start_and_verify(
            &mut self,
            r: &Request,
            _: &Scaffold,
            id: &str,
        ) -> Result<RuntimeIdentity, Reply> {
            self.action()?;
            Ok(RuntimeIdentity {
                registration: r.registration,
                incarnation: r.incarnation,
                container_id: id.into(),
                namespace: NamespaceIdentity {
                    device: 2,
                    inode: 3,
                },
            })
        }
        fn commit_runtime(&mut self, _: &Request, _: &RuntimeIdentity) -> Result<(), Reply> {
            self.action()
        }
    }

    fn request() -> Request {
        serde_json::from_value(serde_json::json!({
            "version": 1,
            "configuration": vec![1; 32],
            "operation": "00000000000000000000000001",
            "provenance": {"daemon_instance": "00000000000000000000000002", "caller": "local_operator"},
            "host": "00000000000000000000000003",
            "target": "synthetic",
            "registration": "00000000000000000000000004",
            "incarnation": "00000000000000000000000005",
            "domain": "00000000000000000000000006"
        })).unwrap()
    }

    #[test]
    fn every_persistence_boundary_refuses_replay_after_error_or_process_restart() {
        // Seven boundaries for each of intent, scaffold, create-pending,
        // container binding, receiver verification and final commit. These
        // are process interruptions / I/O errors, not power-loss durability proof.
        for crash in [false, true] {
            for boundary in 1..=42 {
                let dir = tempfile::tempdir().unwrap();
                let store = Store::fixture(dir.path()).unwrap();
                store.fault.set(Some((boundary, crash)));
                let r = request();
                let mut b = Bootstrap::new(store, Fake::fresh(0));
                let outcome =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| b.create(r.clone())));
                assert_eq!(outcome.is_err(), crash, "boundary {boundary}");
                if !crash {
                    assert_eq!(outcome.unwrap(), Err(Reply::Quarantined));
                    let calls = b.backend.calls;
                    assert_eq!(b.create(r.clone()), Err(Reply::Quarantined));
                    assert_eq!(b.retry(&r), Err(Reply::Quarantined));
                    assert_eq!(b.backend.calls, calls);
                }
                // No create/start/commit is repeated after reopening. A
                // crash after final rename may expose an exact committed
                // result, but returning it must never invoke external work.
                drop(b);
                match Store::fixture(dir.path()) {
                    Err(Reply::Quarantined) => {
                        assert!(dir.path().join("bootstrap.next").exists());
                    }
                    Ok(store) => {
                        let committed = store.load_bootstrap().is_ok_and(|j| j.committed());
                        let mut restarted = Bootstrap::new(store, Fake::fresh(0));
                        assert_eq!(restarted.retry(&r).is_ok(), committed);
                        assert_eq!(restarted.backend.calls, 0);
                        if dir.path().join("bootstrap.json").exists() {
                            assert!(restarted.create(r.clone()).is_err());
                            assert_eq!(restarted.backend.calls, 0);
                        }
                    }
                    Err(other) => panic!("unexpected reopen result: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn post_effect_pre_record_failures_stay_quarantined_after_restart() {
        for fail in 1..=4 {
            let dir = tempfile::tempdir().unwrap();
            let mut fake = Fake::fresh(fail);
            fake.post_effect = true;
            let mut b = Bootstrap::new(Store::fixture(dir.path()).unwrap(), fake);
            let r = request();
            assert_eq!(b.create(r.clone()), Err(Reply::Quarantined));
            assert_eq!(b.backend.effects, fail);
            assert!(b.store.load_bootstrap().unwrap().quarantined());
            drop(b);
            let mut restarted = Bootstrap::new(Store::fixture(dir.path()).unwrap(), Fake::fresh(0));
            assert_eq!(restarted.retry(&r), Err(Reply::Quarantined));
            assert!(restarted.create(r).is_err());
            assert_eq!(restarted.backend.effects, 0);
        }
    }

    #[test]
    fn configuration_drift_fences_committed_retry_and_recovery() {
        for complete in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::fixture(dir.path()).unwrap();
            let r = request();
            let mut b = Bootstrap::new(store, Fake::fresh(0));
            if complete {
                b.create(r.clone()).unwrap();
            } else {
                b.store
                    .begin_bootstrap(&Journal::new(r.clone()).unwrap())
                    .unwrap();
            }
            let calls = b.backend.calls;
            b.backend.configuration_changed = true;
            assert_eq!(b.retry(&r), Err(Reply::Quarantined));
            assert!(b.store.load_bootstrap().unwrap().quarantined());
            assert_eq!(b.backend.calls, calls);
            drop(b);
            let mut recovered = Bootstrap::new(Store::fixture(dir.path()).unwrap(), Fake::fresh(0));
            // Restoring the old configuration cannot clear durable quarantine.
            assert_eq!(recovered.retry(&r), Err(Reply::Quarantined));
            assert_eq!(recovered.backend.calls, 0);
        }
    }

    #[test]
    fn configuration_drift_before_and_after_intent_never_creates_scaffold() {
        let dir = tempfile::tempdir().unwrap();
        let roots = super::super::registration_roots::Roots::fixture(dir.path());
        let r = request();
        let mut changed = Fake::fresh(0);
        changed.configuration_changed = true;
        assert!(Bootstrap::allocate(&roots, r.clone(), changed).is_err());
        assert_eq!(
            std::fs::read_dir(dir.path().join("bootstrap"))
                .unwrap()
                .count(),
            0
        );
        let mut b = Bootstrap::allocate(&roots, r, Fake::fresh(0)).unwrap();
        b.backend.configuration_changed = true;
        assert_eq!(b.create_prepared(), Err(Reply::Quarantined));
        assert_eq!(b.backend.calls, 0);
        assert!(b.store.load_bootstrap().unwrap().quarantined());
    }

    #[test]
    fn fresh_metadata_mints_only_one_create_continuation() {
        let dir = tempfile::tempdir().unwrap();
        let roots = super::super::registration_roots::Roots::fixture(dir.path());
        let r = request();
        let mut b = Bootstrap::allocate(&roots, r.clone(), Fake::fresh(0)).unwrap();
        assert!(Bootstrap::allocate(&roots, r.clone(), Fake::fresh(0)).is_err());
        b.create_prepared().unwrap();
        assert_eq!(b.backend.calls, 4);
        assert_eq!(b.create_prepared(), Err(Reply::Conflict));
        b.retry(&r).unwrap();
        assert_eq!(b.backend.calls, 4);
    }

    #[test]
    fn recovered_initial_metadata_cannot_mint_a_create_continuation() {
        let dir = tempfile::tempdir().unwrap();
        let roots = super::super::registration_roots::Roots::fixture(dir.path());
        let r = request();
        drop(Bootstrap::allocate(&roots, r.clone(), Fake::fresh(0)).unwrap());
        assert!(Bootstrap::allocate(&roots, r.clone(), Fake::fresh(0)).is_err());
        let store = Store::fixture(
            &dir.path()
                .join("bootstrap")
                .join(r.registration.0.to_string()),
        )
        .unwrap();
        let mut recovered = Bootstrap::new(store, Fake::fresh(0));
        assert_eq!(recovered.create_prepared(), Err(Reply::Conflict));
        assert_eq!(recovered.retry(&r), Err(Reply::Quarantined));
        assert_eq!(recovered.backend.calls, 0);
        assert!(recovered.store.load_bootstrap().unwrap().quarantined());
    }

    #[test]
    fn every_external_failure_fences_without_replaying() {
        for fail in 1..=4 {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::fixture(dir.path()).unwrap();
            let mut b = Bootstrap::new(store, Fake::fresh(fail));
            let r = request();
            assert_eq!(b.create(r.clone()), Err(Reply::Quarantined));
            assert!(b.store.load_bootstrap().unwrap().quarantined());
            assert_eq!(b.create(r.clone()), Err(Reply::Quarantined));
            assert_eq!(b.retry(&r), Err(Reply::Quarantined));
            assert_eq!(b.backend.calls, fail);
        }
    }

    #[test]
    fn committed_duplicate_and_recovered_intent_never_repeat_actions() {
        for complete in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::fixture(dir.path()).unwrap();
            let r = request();
            let mut b = Bootstrap::new(store, Fake::fresh(0));
            if complete {
                b.create(r.clone()).unwrap();
            } else {
                b.store
                    .begin_bootstrap(&Journal::new(r.clone()).unwrap())
                    .unwrap();
            }
            let calls = b.backend.calls;
            assert_eq!(b.retry(&r).is_ok(), complete);
            assert_eq!(b.backend.calls, calls);
            assert_eq!(b.create(r), Err(Reply::Conflict));
            assert_eq!(b.backend.calls, calls);
        }
    }
}
