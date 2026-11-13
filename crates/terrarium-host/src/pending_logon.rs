//! Bounded, connection-revocable reservations for Windows credential preparation.
//! Contains no passwords or tokens. Cancelled work keeps its capacity until the
//! non-interruptible native logon returns, preventing cancellation/retry floods.
//! Start commitment is atomic and never holds queue/registry locks across OS
//! calls. Revocation after that boundary cannot undo an in-flight OS creation:
//! the owner stops the child once its handle exists. New admission may return
//! Busy during another start's commit rather than stall the connection reader.

use crate::protocol::SessionId;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

const MAX_EVENTS: usize = 64;
// The CLI sends 256 KiB inputs and the daemon accepts MAX_DATA_PAYLOAD.
// Budget four maximum encoded frames plus one host chunk for terminal/control
// frames. This includes the wire header, unlike a CHUNK_SIZE-only budget.
const MAX_BYTES: usize = 4 * (crate::protocol::MAX_DATA_PAYLOAD + crate::protocol::DATA_HEADER_LEN)
    + crate::protocol::CHUNK_SIZE;
const PREPARING: u8 = 0;
const COMMITTED: u8 = 1;
const CANCELLED: u8 = 2;
const REVOKED: u8 = 3;

pub(crate) enum Event {
    Control(Box<crate::protocol::DaemonFrame>),
    Binary(Vec<u8>),
}

#[derive(Default)]
struct State {
    handed_off: bool,
    bytes: usize,
    queue: VecDeque<(Event, usize)>,
}

#[derive(Default)]
pub(crate) struct Pending {
    phase: AtomicU8,
    state: Mutex<State>,
}

#[derive(Clone, Default)]
pub(crate) struct Reservations(Arc<Mutex<HashMap<SessionId, Arc<Pending>>>>);

pub(crate) enum Routed {
    Absent(Event),
    Queued,
    Cancelled,
    Overflow,
}

pub(crate) struct Lease {
    registry: Reservations,
    pub(crate) id: SessionId,
    pub(crate) pending: Arc<Pending>,
}

impl Reservations {
    pub(crate) fn count(&self) -> usize {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub(crate) fn unpublished(&self, published: impl Fn(SessionId) -> bool) -> usize {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .filter(|id| !published(**id))
            .count()
    }

    pub(crate) fn contains(&self, id: SessionId) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&id)
    }

    /// Caller holds admission and has checked both capacity and duplicate IDs.
    pub(crate) fn reserve(&self, id: SessionId) -> Lease {
        let pending = Arc::new(Pending::default());
        let previous = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, pending.clone());
        assert!(previous.is_none(), "duplicate pending reservation");
        Lease {
            registry: self.clone(),
            id,
            pending,
        }
    }

    pub(crate) fn owns(&self, id: SessionId, owner: Option<&Arc<Pending>>) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .is_some_and(|p| owner.is_some_and(|owner| Arc::ptr_eq(owner, p)))
    }

    pub(crate) fn route(&self, id: SessionId, event: Event, bytes: usize) -> Routed {
        let pending = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .cloned();
        let Some(pending) = pending else {
            return Routed::Absent(event);
        };
        let mut state = pending.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.handed_off {
            return Routed::Absent(event);
        }
        if pending.cancelled() {
            return Routed::Cancelled;
        }
        if state.queue.len() >= MAX_EVENTS || bytes > MAX_BYTES.saturating_sub(state.bytes) {
            pending.revoke();
            state.queue.clear();
            state.bytes = 0;
            return Routed::Overflow;
        }
        state.bytes += bytes;
        state.queue.push_back((event, bytes));
        Routed::Queued
    }

    pub(crate) fn cancel(&self, id: SessionId) -> Option<bool> {
        let pending = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .cloned()?;
        Some(pending.cancel())
    }

    pub(crate) fn cancel_preparing(&self, id: SessionId) -> Option<bool> {
        let pending = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .cloned()?;
        Some(pending.cancel_preparing())
    }

    pub(crate) fn cancel_all(&self) {
        let pending: Vec<_> = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for pending in pending {
            pending.cancel();
        }
    }
}

impl Pending {
    fn revoke(&self) -> bool {
        let old = self
            .phase
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |phase| {
                Some(match phase {
                    PREPARING | CANCELLED => CANCELLED,
                    _ => REVOKED,
                })
            })
            .expect("phase update always has a replacement");
        matches!(old, COMMITTED | REVOKED)
    }

    pub(crate) fn cancel(&self) -> bool {
        let committed = self.revoke();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.queue.clear();
        state.bytes = 0;
        committed
    }

    fn cancel_preparing(&self) -> bool {
        if self
            .phase
            .compare_exchange(PREPARING, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.queue.clear();
        state.bytes = 0;
        true
    }

    pub(crate) fn cancelled(&self) -> bool {
        matches!(self.phase.load(Ordering::Acquire), CANCELLED | REVOKED)
    }

    /// Logical start boundary, not a claim that the OS has created the child.
    /// Cancellation winning this CAS prevents spawn. After it, OS creation may
    /// block: revoke is nonblocking and the child is stopped once available.
    /// No queue or registry lock is held across filesystem/process APIs.
    pub(crate) fn commit(&self) -> bool {
        self.phase
            .compare_exchange(PREPARING, COMMITTED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

impl Lease {
    /// Atomically drain queued events or hand routing back to the active entry.
    pub(crate) fn pop_or_handoff(&self) -> Option<Event> {
        let mut registry = self.registry.0.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.pending.state.lock().unwrap_or_else(|e| e.into_inner());
        // A revoked reservation must remain owned until the worker has cleaned
        // up any published entry. Otherwise its cleanup could hit a reused ID.
        if self.pending.cancelled() {
            return None;
        }
        if let Some((event, bytes)) = state.queue.pop_front() {
            state.bytes -= bytes;
            return Some(event);
        }
        state.handed_off = true;
        if registry
            .get(&self.id)
            .is_some_and(|p| Arc::ptr_eq(p, &self.pending))
        {
            registry.remove(&self.id);
        }
        None
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut registry = self.registry.0.lock().unwrap_or_else(|e| e.into_inner());
        if registry
            .get(&self.id)
            .is_some_and(|p| Arc::ptr_eq(p, &self.pending))
        {
            registry.remove(&self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::DaemonFrame;

    #[test]
    fn mixed_early_frames_keep_order_and_handoff_is_atomic() {
        let registry = Reservations::default();
        let lease = registry.reserve(1);
        assert!(matches!(
            registry.route(1, Event::Binary(vec![1]), 1),
            Routed::Queued
        ));
        assert!(matches!(
            registry.route(
                1,
                Event::Control(Box::new(DaemonFrame::Eof { session: 1 })),
                0
            ),
            Routed::Queued
        ));
        assert!(matches!(
            registry.route(1, Event::Binary(vec![2]), 1),
            Routed::Queued
        ));
        assert!(matches!(lease.pop_or_handoff(), Some(Event::Binary(data)) if data == vec![1]));
        assert!(matches!(
            lease.pop_or_handoff(),
            Some(Event::Control(frame)) if matches!(*frame, DaemonFrame::Eof { .. })
        ));
        assert!(matches!(lease.pop_or_handoff(), Some(Event::Binary(data)) if data == vec![2]));
        assert!(lease.pop_or_handoff().is_none());
        assert!(matches!(
            registry.route(1, Event::Binary(vec![3]), 1),
            Routed::Absent(_)
        ));
    }

    #[test]
    fn cancellation_prevents_commit_but_retains_capacity_until_work_finishes() {
        let registry = Reservations::default();
        let lease = registry.reserve(1);
        assert_eq!(registry.cancel(1), Some(false));
        assert!(!lease.pending.commit());
        assert_eq!(registry.count(), 1);
        drop(lease);
        assert_eq!(registry.count(), 0);
    }

    #[test]
    fn stale_owner_drop_cannot_remove_reused_session() {
        let registry = Reservations::default();
        let old = registry.reserve(1);
        assert!(old.pop_or_handoff().is_none());
        let new = registry.reserve(1);
        drop(old);
        assert!(registry.owns(1, Some(&new.pending)));
    }

    #[test]
    fn cancelled_pop_keeps_ownership_until_cleanup_and_drop() {
        let registry = Reservations::default();
        let lease = registry.reserve(1);
        registry.cancel(1);
        assert!(lease.pop_or_handoff().is_none());
        assert!(registry.owns(1, Some(&lease.pending)));
        drop(lease);
        assert!(!registry.contains(1));
    }

    #[test]
    fn queue_event_and_byte_limits_cancel_instead_of_blocking() {
        let registry = Reservations::default();
        let lease = registry.reserve(1);
        for _ in 0..MAX_EVENTS {
            assert!(matches!(
                registry.route(1, Event::Binary(vec![]), 0),
                Routed::Queued
            ));
        }
        assert!(matches!(
            registry.route(1, Event::Binary(vec![]), 0),
            Routed::Overflow
        ));
        assert!(lease.pending.cancelled());
        let other = registry.reserve(2);
        assert!(matches!(
            registry.route(2, Event::Binary(vec![]), MAX_BYTES + 1),
            Routed::Overflow
        ));
        assert!(other.pending.cancelled());
        assert_eq!(registry.count(), 2);
    }

    #[test]
    fn drained_bytes_do_not_consume_future_queue_capacity() {
        let registry = Reservations::default();
        let lease = registry.reserve(1);
        for _ in 0..3 {
            assert!(matches!(
                registry.route(1, Event::Binary(vec![]), MAX_BYTES),
                Routed::Queued
            ));
            assert!(lease.pop_or_handoff().is_some());
        }
    }

    #[tokio::test]
    async fn delayed_native_work_cannot_commit_after_connection_revoke() {
        let registry = Reservations::default();
        let lease = registry.reserve(1);
        let (finish, blocked) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            blocked.await.unwrap();
            lease.pending.commit()
        });
        registry.cancel_all();
        assert_eq!(registry.count(), 1);
        finish.send(()).unwrap();
        assert!(!task.await.unwrap());
        assert_eq!(registry.count(), 0);
    }
}
