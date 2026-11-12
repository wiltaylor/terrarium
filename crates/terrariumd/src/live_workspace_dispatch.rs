//! Inactive, fake-first registration dispatcher. No wire implementation or
//! lifecycle/admission caller is installed. SQL authority facts may change
//! independently of these owners; allocation and delivery changes may not.
// Until the remaining reviewed partitions supply callers, keep the boundary
// private rather than exposing an unlocked construction path to integrations.
#![allow(dead_code)]

use crate::{
    db::Db,
    error::{ApiError, ApiResult, Code},
    store::live_workspaces::operations,
};
use futures_util::FutureExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc, time::Duration};
use terrarium_core::live_workspace::{
    AttachmentId, DaemonPhase, Envelope, OperationId, Provenance, RegistrationId, Reply,
};
use tokio::{
    sync::{Semaphore, mpsc, oneshot},
    task::JoinHandle,
    time::Instant,
};

type StateLock = Arc<nix::fcntl::Flock<std::fs::File>>;
type Fences = Arc<std::sync::Mutex<Vec<JoinHandle<()>>>>;
type Exchange<'a> = Pin<Box<dyn Future<Output = ApiResult<(Envelope, Reply)>> + Send + 'a>>;

/// A fake exchange returns the processed envelope, not a reconstructed one.
/// Real authenticated transport belongs to separately reviewed partitions.
trait Transport: Send + Sync + 'static {
    fn exchange(&self, frozen: Arc<[u8]>) -> Exchange<'_>;
}

enum Command {
    Drain,
    Grant(Box<operations::Grant>),
    Revoke {
        provenance: Provenance,
        operation: OperationId,
        attachment: AttachmentId,
    },
}

struct Queued {
    command: Command,
    deadline: Instant,
    reply: oneshot::Sender<ApiResult<()>>,
}

struct Entry {
    queue: mpsc::Sender<Queued>,
    poison: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

/// Constructed only by Daemon after its authoritative flock and startup fence.
/// No eviction: even a dead/poisoned worker keeps its registration slot. The
/// protected, nonreplaceable local state directory/inode is administrative
/// trust, not distributed fencing against an administrator or aliased DB.
pub(super) struct Dispatcher {
    db: Db,
    lock: StateLock,
    entries: BTreeMap<RegistrationId, Entry>,
    permits: Arc<Semaphore>,
    fences: Fences,
}

impl Dispatcher {
    pub(super) fn new(db: Db, lock: StateLock) -> Self {
        Self {
            db,
            lock,
            entries: BTreeMap::new(),
            permits: Arc::new(Semaphore::new(16)),
            fences: Arc::default(),
        }
    }

    fn submit(
        &mut self,
        registration: RegistrationId,
        command: Command,
        transport: Arc<dyn Transport>,
    ) -> ApiResult<oneshot::Receiver<ApiResult<()>>> {
        if let Command::Grant(grant) = &command
            && grant.registration != registration
        {
            return Err(ApiError::new(Code::Conflict, "wrong grant owner"));
        }
        let full = self.entries.len() == 128;
        if let std::collections::btree_map::Entry::Vacant(entry) = self.entries.entry(registration)
        {
            if full {
                return Err(busy());
            }
            let (queue, receiver) = mpsc::channel(32);
            let poison = Arc::new(AtomicBool::new(false));
            let owner = Owner {
                db: self.db.clone(),
                lock: self.lock.clone(),
                registration,
                poison: poison.clone(),
                permits: self.permits.clone(),
                fences: self.fences.clone(),
                transport,
            };
            let task = tokio::spawn(owner.run(receiver));
            entry.insert(Entry {
                queue,
                poison,
                task,
            });
        }
        let entry = &self.entries[&registration];
        if entry.poison.load(Ordering::SeqCst) || entry.task.is_finished() {
            return Err(fenced());
        }
        let (reply, wait) = oneshot::channel();
        entry
            .queue
            .try_send(Queued {
                command,
                deadline: Instant::now() + Duration::from_secs(5),
                reply,
            })
            .map_err(|_| busy())?;
        Ok(wait)
    }

    pub(super) fn stop(&mut self) {
        for entry in self.entries.values() {
            entry.task.abort();
        }
    }

    pub(super) async fn stop_and_wait(&mut self) {
        self.stop();
        for (_, entry) in std::mem::take(&mut self.entries) {
            let _ = entry.task.await;
        }
        // Workers have ended, so no new fence can be registered. Never carry
        // the bookkeeping mutex over await or into a database transaction.
        let fences = std::mem::take(&mut *self.fences.lock().unwrap_or_else(|e| e.into_inner()));
        for fence in fences {
            let _ = fence.await;
        }
    }
}

impl Drop for Dispatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

fn busy() -> ApiError {
    ApiError::new(Code::ResourceBusy, "workspace dispatch capacity exhausted")
}
fn fenced() -> ApiError {
    ApiError::new(Code::Conflict, "workspace dispatch poisoned until restart")
}

struct Owner {
    db: Db,
    lock: StateLock,
    registration: RegistrationId,
    poison: Arc<AtomicBool>,
    permits: Arc<Semaphore>,
    fences: Fences,
    transport: Arc<dyn Transport>,
}

/// Arm before allocation. Drop/panic sets poison synchronously, before the
/// owner can disappear, then schedules best-effort FULL persistence. Persistence
/// failure never clears poison or replaces the owner. The fence task and each
/// blocking write retain the authoritative flock until they actually finish.
struct InFlight<'a> {
    owner: &'a Owner,
    selected: Option<OperationId>,
    armed: bool,
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.owner.poison.store(true, Ordering::SeqCst);
        let db = self.owner.db.clone();
        let lock = self.owner.lock.clone();
        let registration = self.owner.registration;
        let selected = self.selected;
        // During runtime teardown startup quarantine is the durable fallback.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let task = runtime.spawn(async move {
                let result = db
                    .write_full("system", move |t| {
                        let _lock = lock;
                        operations::quarantine_dispatch(t, registration, selected)
                    })
                    .await;
                if let Err(error) = result {
                    tracing::error!(%error, "workspace poison persistence failed; owner retained");
                }
            });
            self.owner
                .fences
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(task);
        }
    }
}

impl Owner {
    async fn run(self, mut receiver: mpsc::Receiver<Queued>) {
        while let Some(queued) = receiver.recv().await {
            if queued.reply.is_closed() {
                continue;
            } // No intent for canceled queued callers.
            if queued.deadline <= Instant::now() {
                let _ = queued.reply.send(Err(busy()));
                continue;
            }
            let mut guard = InFlight {
                owner: &self,
                selected: None,
                armed: true,
            };
            // Panic and task cancellation share the same fail-closed guard.
            let result = std::panic::AssertUnwindSafe(self.execute(queued.command, &mut guard))
                .catch_unwind()
                .await;
            match result {
                Ok(Ok(())) => {
                    guard.armed = false;
                    let _ = queued.reply.send(Ok(()));
                }
                Ok(Err(error)) => {
                    let _ = queued.reply.send(Err(error));
                }
                Err(_) => {
                    let _ = queued.reply.send(Err(fenced()));
                }
            }
            drop(guard);
            if self.poison.load(Ordering::SeqCst) {
                break;
            }
        }
    }

    async fn execute(&self, command: Command, guard: &mut InFlight<'_>) -> ApiResult<()> {
        let registration = self.registration;
        let lock = self.lock.clone();
        let mut selected = self
            .db
            .write_full("system", move |t| {
                let _lock = lock;
                match command {
                    Command::Drain => {}
                    Command::Grant(grant) => {
                        operations::allocate_grant(t, &grant)?;
                    }
                    Command::Revoke {
                        provenance,
                        operation,
                        attachment,
                    } => {
                        operations::allocate_revoke(
                            t,
                            &provenance,
                            operation,
                            registration,
                            attachment,
                        )?;
                    }
                }
                operations::next(t, registration)
            })
            .await?;
        while let Some(stored) = selected {
            let operation = stored.envelope.original.operation;
            guard.selected = Some(operation);
            if stored.phase == DaemonPhase::Intent {
                // Freeze once before any await. No retry or superseded rewrite
                // after an ambiguous send. Runnable owners alone take permits.
                let frozen: Arc<[u8]> = serde_json::to_vec(&stored.envelope)
                    .map_err(|_| fenced())?
                    .into();
                let permit = tokio::time::timeout(Duration::from_secs(5), self.permits.acquire())
                    .await
                    .map_err(|_| busy())?
                    .map_err(|_| fenced())?;
                let (receipt, result) =
                    tokio::time::timeout(Duration::from_secs(10), self.transport.exchange(frozen))
                        .await
                        .map_err(|_| fenced())??;
                drop(permit);
                if receipt != stored.envelope {
                    return Err(fenced());
                }
                let lock = self.lock.clone();
                let result = self
                    .db
                    .write_full("system", move |t| {
                        let _lock = lock;
                        operations::acknowledge(t, &receipt, result)
                    })
                    .await?;
                if result == Reply::Quarantined {
                    return Err(fenced());
                }
            }
            let lock = self.lock.clone();
            selected = self
                .db
                .write_full("system", move |t| {
                    let _lock = lock;
                    operations::commit_runtime(t, operation)?;
                    operations::next(t, registration)
                })
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
