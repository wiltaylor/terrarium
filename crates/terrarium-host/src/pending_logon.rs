//! Bounded, connection-revocable reservations for Windows credential preparation.
//! Contains no passwords or tokens. Cancelled work keeps its capacity until the
//! non-interruptible native logon returns, preventing cancellation/retry floods.

use crate::protocol::SessionId;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

const MAX_EVENTS: usize = 64;
const MAX_BYTES: usize = 8 * crate::protocol::CHUNK_SIZE;

pub(crate) enum Event {
    Control(Box<crate::protocol::DaemonFrame>),
    Binary(Vec<u8>),
}

#[derive(Default)]
pub(crate) struct State {
    cancelled: bool,
    committing: bool,
    bytes: usize,
    queue: VecDeque<(Event, usize)>,
}

#[derive(Default)]
pub(crate) struct Pending {
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
        let registry = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let Some(pending) = registry.get(&id) else {
            return Routed::Absent(event);
        };
        let mut state = pending.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.cancelled {
            return Routed::Cancelled;
        }
        if state.queue.len() >= MAX_EVENTS || bytes > MAX_BYTES.saturating_sub(state.bytes) {
            state.cancelled = true;
            state.queue.clear();
            state.bytes = 0;
            return Routed::Overflow;
        }
        state.bytes += bytes;
        state.queue.push_back((event, bytes));
        Routed::Queued
    }

    pub(crate) fn cancel(&self, id: SessionId) -> Option<bool> {
        let registry = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let pending = registry.get(&id)?;
        Some(pending.cancel())
    }

    pub(crate) fn cancel_all(&self) {
        for pending in self.0.lock().unwrap_or_else(|e| e.into_inner()).values() {
            pending.cancel();
        }
    }
}

impl Pending {
    pub(crate) fn cancel(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.cancelled = true;
        state.queue.clear();
        state.bytes = 0;
        state.committing
    }

    pub(crate) fn cancelled(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cancelled
    }

    /// Linearize process-start commitment against pending cancellation. The
    /// short synchronous spawn runs while holding this guard. A cancellation
    /// that wins first cannot start a child. Once spawn has committed, normal
    /// session controls stop the child instead of cancelling an earlier logon.
    pub(crate) fn commit(&self) -> Option<std::sync::MutexGuard<'_, State>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.cancelled {
            return None;
        }
        state.committing = true;
        Some(state)
    }
}

impl Lease {
    /// Atomically drain queued events or hand routing back to the active entry.
    pub(crate) fn pop_or_handoff(&self) -> Option<Event> {
        let mut registry = self.registry.0.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.pending.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.cancelled
            && let Some((event, bytes)) = state.queue.pop_front()
        {
            state.bytes -= bytes;
            return Some(event);
        }
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
            registry.route(1, Event::Control(Box::new(DaemonFrame::Eof { session: 1 })), 0),
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
        assert!(lease.pending.commit().is_none());
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
            lease.pending.commit().map(|_| ()).is_some()
        });
        registry.cancel_all();
        assert_eq!(registry.count(), 1);
        finish.send(()).unwrap();
        assert!(!task.await.unwrap());
        assert_eq!(registry.count(), 0);
    }
}
