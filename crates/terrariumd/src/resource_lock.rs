//! Observations of the existing resource admission mutex, never admission policy.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{OwnedMutexGuard, TryLockError};
use tokio::time::Instant;

#[derive(Clone, Copy, Debug)]
pub enum ResourceOperation {
    Command,
    Exec,
    Copy,
    Job,
    Lifecycle,
    Grant,
    Extend,
    Finish,
    Cancel,
    Sweep,
    Reconcile,
    Capacity,
    TargetUpdate,
}

impl ResourceOperation {
    fn tags(self) -> (&'static str, &'static str) {
        use ResourceOperation::*;
        match self {
            Command => ("command", "admission"),
            Exec => ("exec", "admission"),
            Copy => ("copy", "admission"),
            Job => ("job", "admission"),
            Lifecycle => ("lifecycle", "control_plane"),
            Grant => ("grant", "control_plane"),
            Extend => ("extend", "control_plane"),
            Finish => ("finish", "control_plane"),
            Cancel => ("cancel", "control_plane"),
            Sweep => ("sweep", "control_plane"),
            Reconcile => ("reconcile", "control_plane"),
            Capacity => ("capacity", "control_plane"),
            TargetUpdate => ("target_update", "control_plane"),
        }
    }
}

struct Holder {
    generation: Arc<()>,
    operation: ResourceOperation,
    acquired: Instant,
}

pub struct ObservedResourceLock {
    admission: Arc<tokio::sync::Mutex<()>>,
    holder: Mutex<Option<Holder>>,
    token: u64,
}

impl Default for ObservedResourceLock {
    fn default() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            admission: Arc::default(),
            holder: Mutex::default(),
            token: NEXT
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
                .unwrap_or_else(|_| panic!("resource lock token space exhausted")),
        }
    }
}

pub struct ObservedResourceGuard {
    lock: Arc<ObservedResourceLock>,
    generation: Arc<()>,
    admission: Option<OwnedMutexGuard<()>>,
}

impl std::fmt::Debug for ObservedResourceGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ObservedResourceGuard")
    }
}

impl ObservedResourceLock {
    pub async fn lock_owned(
        self: Arc<Self>,
        operation: ResourceOperation,
    ) -> ObservedResourceGuard {
        let generation = Arc::new(());
        let admission = self.admission.clone().lock_owned().await;
        self.install(admission, generation, operation)
    }

    pub fn try_lock_owned(
        self: Arc<Self>,
        operation: ResourceOperation,
    ) -> Result<ObservedResourceGuard, TryLockError> {
        let generation = Arc::new(());
        let admission = self.admission.clone().try_lock_owned()?;
        Ok(self.install(admission, generation, operation))
    }

    fn install(
        self: Arc<Self>,
        admission: OwnedMutexGuard<()>,
        generation: Arc<()>,
        operation: ResourceOperation,
    ) -> ObservedResourceGuard {
        {
            match self.holder.lock() {
                Ok(mut slot) => {
                    *slot = Some(Holder {
                        generation: generation.clone(),
                        operation,
                        acquired: Instant::now(),
                    });
                }
                Err(poison) => *poison.into_inner() = None,
            }
        }
        ObservedResourceGuard {
            lock: self,
            generation,
            admission: Some(admission),
        }
    }

    fn snapshot(&self, now: Instant) -> Option<(ResourceOperation, u64, bool)> {
        match self.holder.lock() {
            Ok(slot) => slot.as_ref().map(|h| {
                let (age, capped) = elapsed(now, h.acquired);
                (h.operation, age, capped)
            }),
            Err(poison) => {
                *poison.into_inner() = None;
                None
            }
        }
    }

    pub(crate) fn log_timeout(&self, started: Instant) {
        let now = Instant::now();
        let holder = self.snapshot(now);
        let (wait_ms, wait_capped) = elapsed(now, started);
        let (operation, phase, age, age_capped) = match holder {
            Some((op, age, capped)) => {
                let (op, phase) = op.tags();
                (op, phase, Some(age), Some(capped))
            }
            None => ("unknown", "unknown", None, None),
        };
        // No parent span: names and payloads in caller spans must not enter
        // this diagnostic. Snapshot has released metadata before rendering.
        tracing::warn!(parent: None,
            lock_token = self.token,
            holder_operation = operation,
            holder_phase = phase,
            holder_age_ms = age,
            holder_age_capped = age_capped,
            wait_ms,
            wait_capped,
            "host admission timed out; holder sampled at timeout"
        );
    }
}

fn elapsed(now: Instant, start: Instant) -> (u64, bool) {
    let millis = now
        .checked_duration_since(start)
        .unwrap_or_default()
        .as_millis();
    let cap = 86_400_000_u128;
    (millis.min(cap) as u64, millis > cap)
}

impl Drop for ObservedResourceGuard {
    fn drop(&mut self) {
        let mut slot = match self.lock.holder.lock() {
            Ok(slot) => slot,
            Err(poison) => {
                let mut slot = poison.into_inner();
                *slot = None;
                slot
            }
        };
        if slot
            .as_ref()
            .is_some_and(|h| Arc::ptr_eq(&h.generation, &self.generation))
        {
            *slot = None;
        }
        // Explicitly unlock admission before unlocking metadata. A successor
        // can acquire admission but cannot publish until this slot is clear.
        drop(self.admission.take());
        drop(slot);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::pin::pin;
    use std::time::Duration;

    // Poll once to establish queue order without enabling dependency features.
    macro_rules! poll {
        ($future:expr) => {
            std::future::poll_fn(|cx| {
                std::task::Poll::Ready(std::future::Future::poll(($future).as_mut(), cx))
            })
            .await
        };
    }

    #[tokio::test(start_paused = true)]
    async fn fifo_cancel_timeout_and_independent_progress() {
        let lock = Arc::new(ObservedResourceLock::default());
        let other = Arc::new(ObservedResourceLock::default());
        let guard = lock.clone().lock_owned(ResourceOperation::Grant).await;
        let mut first = Box::pin(lock.clone().lock_owned(ResourceOperation::Exec));
        let mut cancelled = Box::pin(lock.clone().lock_owned(ResourceOperation::Copy));
        let mut last = Box::pin(lock.clone().lock_owned(ResourceOperation::Job));
        assert!(poll!(&mut first).is_pending());
        assert!(poll!(&mut cancelled).is_pending());
        assert!(poll!(&mut last).is_pending());
        assert!(
            other
                .clone()
                .try_lock_owned(ResourceOperation::Command)
                .is_ok()
        );
        drop(cancelled);
        assert_eq!(lock.snapshot(Instant::now()).unwrap().0.tags().0, "grant");
        let timed = tokio::time::timeout(
            Duration::from_millis(500),
            lock.clone().lock_owned(ResourceOperation::Command),
        );
        let mut timed = Box::pin(timed);
        assert!(poll!(&mut timed).is_pending());
        tokio::time::advance(Duration::from_millis(501)).await;
        assert!(timed.await.is_err());
        assert_eq!(lock.snapshot(Instant::now()).unwrap().0.tags().0, "grant");
        drop(guard);
        // Last was enqueued after first, irrespective of its polling order.
        assert!(poll!(&mut last).is_pending());
        let first = first.await;
        assert_eq!(lock.snapshot(Instant::now()).unwrap().0.tags().0, "exec");
        drop(first);
        let last = last.await;
        assert_eq!(lock.snapshot(Instant::now()).unwrap().0.tags().0, "job");
        drop(last);
        assert!(lock.snapshot(Instant::now()).is_none());
        assert!(lock.try_lock_owned(ResourceOperation::Command).is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn release_before_deadline_acquires_once_and_sample_tracks_handoff() {
        let lock = Arc::new(ObservedResourceLock::default());
        let old = lock.clone().lock_owned(ResourceOperation::Grant).await;
        let acquire = tokio::time::timeout(
            Duration::from_millis(500),
            lock.clone().lock_owned(ResourceOperation::Command),
        );
        let mut acquire = pin!(acquire);
        assert!(poll!(&mut acquire).is_pending());
        tokio::time::advance(Duration::from_millis(100)).await;
        assert_eq!(lock.snapshot(Instant::now()).unwrap().0.tags().0, "grant");
        drop(old);
        assert!(lock.snapshot(Instant::now()).is_none());
        let new = acquire.await.unwrap();
        let (operation, age, capped) = lock.snapshot(Instant::now()).unwrap();
        assert_eq!(operation.tags().0, "command");
        assert_eq!((age, capped), (0, false));
        assert!(
            lock.clone()
                .try_lock_owned(ResourceOperation::Copy)
                .is_err()
        );
        drop(new);
        assert!(lock.snapshot(Instant::now()).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn every_operation_error_unwind_and_cancel_clear_synchronously() {
        use ResourceOperation::*;
        fn fail(_guard: ObservedResourceGuard) -> Result<(), ()> {
            Err(())
        }
        let lock = Arc::new(ObservedResourceLock::default());
        for operation in [
            Command,
            Exec,
            Copy,
            Job,
            Lifecycle,
            Grant,
            Extend,
            Finish,
            Cancel,
            Sweep,
            Reconcile,
            Capacity,
            TargetUpdate,
        ] {
            let guard = lock.clone().lock_owned(operation).await;
            assert_eq!(
                lock.snapshot(Instant::now()).unwrap().0.tags(),
                operation.tags()
            );
            let result = fail(guard);
            assert!(result.is_err());
            assert!(lock.snapshot(Instant::now()).is_none());
            let (installed, ready) = tokio::sync::oneshot::channel();
            let held = lock.clone();
            let task = tokio::spawn(async move {
                let _guard = held.lock_owned(operation).await;
                installed.send(()).unwrap();
                std::future::pending::<()>().await;
            });
            ready.await.unwrap();
            assert_eq!(
                lock.snapshot(Instant::now()).unwrap().0.tags(),
                operation.tags()
            );
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            assert!(lock.snapshot(Instant::now()).is_none());
        }
        let guard = lock.clone().lock_owned(Grant).await;
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = guard;
                panic!("fixture unwind");
            }))
            .is_err()
        );
        assert!(lock.snapshot(Instant::now()).is_none());
        let (installed, ready) = tokio::sync::oneshot::channel();
        let held = lock.clone();
        let task = tokio::spawn(async move {
            let _guard = held.lock_owned(Lifecycle).await;
            installed.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        ready.await.unwrap();
        assert!(lock.snapshot(Instant::now()).is_some());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(lock.snapshot(Instant::now()).is_none());
        assert!(lock.try_lock_owned(Command).is_ok());
    }

    #[test]
    fn poisoned_metadata_is_erased_and_never_trusted_again() {
        let lock = Arc::new(ObservedResourceLock::default());
        let guard = lock
            .clone()
            .try_lock_owned(ResourceOperation::Grant)
            .unwrap();
        let poisoned = lock.clone();
        assert!(
            std::thread::spawn(move || {
                let _slot = poisoned.holder.lock().unwrap();
                panic!("fixture poison");
            })
            .join()
            .is_err()
        );
        assert!(lock.snapshot(Instant::now()).is_none());
        drop(guard);
        let fresh = lock
            .clone()
            .try_lock_owned(ResourceOperation::Exec)
            .unwrap();
        assert!(lock.snapshot(Instant::now()).is_none());
        drop(fresh);
        assert!(lock.try_lock_owned(ResourceOperation::Copy).is_ok());
    }

    #[test]
    fn handoff_gap_is_unknown_and_old_generation_cannot_erase_successor() {
        let lock = Arc::new(ObservedResourceLock::default());
        let old = lock
            .clone()
            .try_lock_owned(ResourceOperation::Grant)
            .unwrap();
        let old_generation = old.generation.clone();
        let metadata = lock.holder.lock().unwrap();
        let (release, released) = std::sync::mpsc::channel();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let drop_barrier = barrier.clone();
        let dropping = std::thread::spawn(move || {
            drop_barrier.wait();
            drop(old);
            release.send(()).unwrap();
        });
        barrier.wait();
        // Old Drop cannot unlock admission until it can clear metadata.
        assert!(lock.admission.clone().try_lock_owned().is_err());
        drop(metadata);
        released.recv().unwrap();
        // Stage the acquisition-to-install gap using private test access.
        let admission = lock.admission.clone().try_lock_owned().unwrap();
        assert!(lock.snapshot(Instant::now()).is_none());
        let new = lock
            .clone()
            .install(admission, Arc::new(()), ResourceOperation::Exec);
        // A stale cleanup token must not remove the current generation.
        drop(ObservedResourceGuard {
            lock: lock.clone(),
            generation: old_generation,
            admission: None,
        });
        assert_eq!(lock.snapshot(Instant::now()).unwrap().0.tags().0, "exec");
        dropping.join().unwrap();
        drop(new);
        assert!(lock.snapshot(Instant::now()).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn monotonic_ages_are_bounded_and_tokens_stable_distinct() {
        let lock = Arc::new(ObservedResourceLock::default());
        let other = ObservedResourceLock::default();
        assert_ne!(lock.token, other.token);
        assert_eq!(lock.token, lock.clone().token);
        let now = Instant::now();
        assert_eq!(elapsed(now, now + Duration::from_secs(1)), (0, false));
        let guard = lock.clone().lock_owned(ResourceOperation::Sweep).await;
        tokio::time::advance(Duration::from_secs(86_401)).await;
        let (_, age, capped) = lock.snapshot(Instant::now()).unwrap();
        assert_eq!((age, capped), (86_400_000, true));
        drop(guard);
        assert!(lock.snapshot(Instant::now()).is_none());
    }

    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn complete_rendered_diagnostic_excludes_inherited_private_fields() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let capture = Capture(output.clone());
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || capture.clone())
            .finish();
        let lock = Arc::new(ObservedResourceLock::default());
        let guard = lock
            .clone()
            .lock_owned(ResourceOperation::TargetUpdate)
            .await;
        let start = Instant::now();
        tokio::time::advance(Duration::from_secs(86_401)).await;
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                "private_context",
                host = "host:host-canary",
                target = "target-canary",
                project = "project-canary",
                agent = "agent-canary",
                argv = "argv-canary",
                env = "env-canary",
                cwd = "cwd-canary",
                credential = "credential-canary",
                stderr = "stderr-canary",
                hook_payload = "payload-canary",
                reservation = "reservation-canary"
            );
            let _entered = span.enter();
            lock.log_timeout(start);
            drop(guard);
            lock.log_timeout(start);
        });
        let rendered = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert_eq!(rendered.lines().count(), 2, "{rendered}");
        assert!(!rendered.contains("canary"), "{rendered}");
        assert!(!rendered.contains("private_context"), "{rendered}");
        assert!(
            rendered.contains("holder_operation=\"target_update\""),
            "{rendered}"
        );
        assert!(
            rendered.contains("holder_phase=\"control_plane\""),
            "{rendered}"
        );
        assert!(rendered.contains("holder_age_ms=86400000"), "{rendered}");
        assert!(rendered.contains("holder_age_capped=true"), "{rendered}");
        let unknown = rendered.lines().last().unwrap();
        assert!(unknown.contains("holder_operation=\"unknown\""));
        assert!(!unknown.contains("holder_age_ms"));
        assert!(unknown.contains("wait_ms=86400000"));
        assert!(unknown.contains("wait_capped=true"));
        assert!(unknown.contains(&format!("lock_token={}", lock.token)));
    }
}
