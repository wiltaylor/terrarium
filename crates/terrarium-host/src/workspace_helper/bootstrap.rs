//! Ordered bootstrap execution. No production backend or service route is
//! selected here. Incomplete recovery is deliberately quarantine-only.

use super::Store;
use terrarium_core::live_workspace::bootstrap::{Journal, Request, Scaffold, Step};
use terrarium_core::live_workspace::{Reply, RuntimeIdentity};

/// Implementations must retain pinned scaffold/runtime FDs for the entire
/// operation. Paths and Docker options are helper-selected, never request data.
pub trait Backend {
    /// Refuse existing children and runtimes, including apparently empty ones.
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use terrarium_core::live_workspace::{NamespaceIdentity, SourceIdentity};

    struct Fake {
        calls: usize,
        fail: usize,
    }

    impl Fake {
        fn action(&mut self) -> Result<(), Reply> {
            self.calls += 1;
            if self.calls == self.fail {
                Err(Reply::Failed)
            } else {
                Ok(())
            }
        }
    }

    impl Backend for Fake {
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
    fn fresh_metadata_mints_only_one_create_continuation() {
        let dir = tempfile::tempdir().unwrap();
        let roots = super::super::registration_roots::Roots::fixture(dir.path());
        let r = request();
        let mut b = Bootstrap::allocate(&roots, r.clone(), Fake { calls: 0, fail: 0 }).unwrap();
        assert!(Bootstrap::allocate(&roots, r.clone(), Fake { calls: 0, fail: 0 }).is_err());
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
        drop(Bootstrap::allocate(&roots, r.clone(), Fake { calls: 0, fail: 0 }).unwrap());
        assert!(Bootstrap::allocate(&roots, r.clone(), Fake { calls: 0, fail: 0 }).is_err());
        let store = Store::fixture(
            &dir.path()
                .join("bootstrap")
                .join(r.registration.0.to_string()),
        )
        .unwrap();
        let mut recovered = Bootstrap::new(store, Fake { calls: 0, fail: 0 });
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
            let mut b = Bootstrap::new(store, Fake { calls: 0, fail });
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
            let mut b = Bootstrap::new(store, Fake { calls: 0, fail: 0 });
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
        journal.recover_without_evidence();
        self.store.save_bootstrap(&journal)?;
        journal.retry(request)
    }

    fn advance(&mut self, journal: &mut Journal, step: Step) -> Result<(), Reply> {
        journal.advance(step)?;
        self.store.save_bootstrap(journal)
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
        self.backend.check_absent(&request)?;
        self.fenced = true;
        self.store.begin_bootstrap(&journal)?;
        self.execute(request, journal)
    }

    fn execute(&mut self, request: Request, mut journal: Journal) -> Result<(), Reply> {
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
            journal.recover_without_evidence();
            // Failure to persist quarantine never permits another action in
            // this instance. An incomplete durable journal fences on restart.
            let _ = self.store.save_bootstrap(&journal);
            return Err(Reply::Quarantined);
        }
        self.fenced = false;
        Ok(())
    }
}
