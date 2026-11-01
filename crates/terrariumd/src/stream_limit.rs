//! The stream limit of a self-hosted subscription (PLAN §16,
//! `terrarium_core::selfhosted::SelfHosted::max_concurrent`): how many
//! agents on the subscription may run a turn at the same time.
//!
//! The gate is in memory. The supervisor (`crate::runner_supervisor`)
//! asks it before it types work into an idle agent: a turn, the recap
//! turn, a push pointer, a compaction (a compaction counts as a stream).
//!
//! - **Take.** An agent that holds a slot goes on. An agent with no slot
//!   takes a free one, but only when no other agent waits before it
//!   (FIFO across agents). Else it joins the queue; its work waits in the
//!   supervisor, and the agent shows `idle` with the reason
//!   [`REASON_PREFIX`] (`stream_limit: waiting for a free stream on
//!   <sub>`).
//! - **Release.** The end of a turn (the agent rests: `idle` or
//!   `waiting`), the end of a compaction with no work after it, a stop, a
//!   crash, and an idle stop free the slot. The free slot goes to the
//!   first agent in the queue; the supervisor then types its work. An
//!   agent that still has work joins the end of the queue again, so a busy
//!   agent does not keep the slot from the others.
//! - **Busy.** An agent that the runner reports `working` counts, also
//!   past the limit (a turn that the operator typed in the terminal, or a
//!   turn that ran when the daemon started). So the count after a daemon
//!   restart comes from the turns that run.
//! - A turn that never ends keeps its slot until the agent stops (the
//!   operator's stop, or `stop-all` on the subscription). There is no
//!   stall timer.
//!
//! The limits come from the store ([`refresh`]): at the start, and after
//! each write of a subscription.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard};

use serde::Serialize;
use terrarium_core::AgentId;

/// The status reason of an agent whose work waits for a free stream.
pub const REASON_PREFIX: &str = "stream_limit";

/// The status reason of an agent that waits for a stream of `sub`.
pub fn waiting_reason(sub: &str) -> String {
    format!("{REASON_PREFIX}: waiting for a free stream on `{sub}`")
}

/// The answer of [`StreamGate::take`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Take {
    /// The agent may run a turn now (it holds a slot, or the subscription
    /// has no limit).
    Go,
    /// The agent waits. `new`: it joined the queue with this call.
    Wait { new: bool },
}

/// The streams of one subscription, for the API.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct StreamUsage {
    pub max: u32,
    pub in_use: usize,
    pub waiting: usize,
}

#[derive(Debug, Default)]
struct Book {
    /// The agents that hold a slot, in the order they took it.
    active: Vec<AgentId>,
    /// The agents that wait, first in first out.
    waiting: VecDeque<AgentId>,
}

#[derive(Debug, Default)]
struct State {
    limits: HashMap<String, u32>,
    books: HashMap<String, Book>,
}

impl State {
    /// Give free slots of `sub` to the agents that wait, in order.
    fn grant(&mut self, sub: &str) -> Vec<AgentId> {
        let Some(max) = self.limits.get(sub).copied() else {
            // No limit any more: every agent that waits goes.
            return self
                .books
                .get_mut(sub)
                .map(|b| b.waiting.drain(..).collect())
                .unwrap_or_default();
        };
        let Some(book) = self.books.get_mut(sub) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        while book.active.len() < max as usize {
            let Some(next) = book.waiting.pop_front() else {
                break;
            };
            book.active.push(next);
            out.push(next);
        }
        out
    }
}

/// The gate of every limited subscription (`UsageRuntime::streams`).
#[derive(Debug, Default)]
pub struct StreamGate(Mutex<State>);

impl StreamGate {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Set the limit of each subscription (no entry: no limit). Returns
    /// the agents that may go now (a raised or removed limit).
    pub fn set_limits(&self, limits: HashMap<String, u32>) -> Vec<AgentId> {
        let mut s = self.lock();
        s.limits = limits;
        let subs: Vec<String> = s.books.keys().cloned().collect();
        let mut out = Vec::new();
        for sub in subs {
            out.extend(s.grant(&sub));
        }
        out
    }

    /// The limit of `sub`, when it has one.
    pub fn limit(&self, sub: &str) -> Option<u32> {
        self.lock().limits.get(sub).copied()
    }

    /// May `agent` on `sub` run a turn now? See the module doc.
    pub fn take(&self, sub: Option<&str>, agent: AgentId) -> Take {
        let Some(sub) = sub else {
            return Take::Go;
        };
        let mut s = self.lock();
        let Some(max) = s.limits.get(sub).copied() else {
            return Take::Go;
        };
        let book = s.books.entry(sub.to_owned()).or_default();
        if book.active.contains(&agent) {
            return Take::Go;
        }
        let first = book.waiting.front().is_none_or(|w| *w == agent);
        if book.active.len() < max as usize && first {
            book.waiting.retain(|w| *w != agent);
            book.active.push(agent);
            return Take::Go;
        }
        if book.waiting.contains(&agent) {
            return Take::Wait { new: false };
        }
        book.waiting.push_back(agent);
        Take::Wait { new: true }
    }

    /// [`StreamGate::take`] with no queue: `true` and a slot only when one
    /// is free and no agent waits (a compaction, which the idle pass tries
    /// again later).
    pub fn take_if_free(&self, sub: Option<&str>, agent: AgentId) -> bool {
        let Some(sub) = sub else {
            return true;
        };
        let mut s = self.lock();
        let Some(max) = s.limits.get(sub).copied() else {
            return true;
        };
        let book = s.books.entry(sub.to_owned()).or_default();
        if book.active.contains(&agent) {
            return true;
        }
        if book.active.len() < max as usize && book.waiting.is_empty() {
            book.active.push(agent);
            return true;
        }
        false
    }

    /// The runner reports `agent` working: it counts, also past the limit.
    pub fn note_busy(&self, sub: Option<&str>, agent: AgentId) {
        let Some(sub) = sub else {
            return;
        };
        let mut s = self.lock();
        if !s.limits.contains_key(sub) {
            return;
        }
        let book = s.books.entry(sub.to_owned()).or_default();
        book.waiting.retain(|w| *w != agent);
        if !book.active.contains(&agent) {
            book.active.push(agent);
        }
    }

    /// `agent` ended its turn: its slot is free. Its place in a queue
    /// stays. Returns the agents that got a slot.
    pub fn release(&self, agent: AgentId) -> Vec<AgentId> {
        let mut s = self.lock();
        let subs: Vec<String> = s
            .books
            .iter_mut()
            .filter_map(|(k, b)| {
                let before = b.active.len();
                b.active.retain(|a| *a != agent);
                (b.active.len() != before).then(|| k.clone())
            })
            .collect();
        let mut out = Vec::new();
        for sub in subs {
            out.extend(s.grant(&sub));
        }
        out
    }

    /// `agent` stopped (a stop, a crash, an idle stop): it leaves every
    /// slot and every queue. Returns the agents that got a slot.
    pub fn forget(&self, agent: AgentId) -> Vec<AgentId> {
        let mut s = self.lock();
        let subs: Vec<String> = s
            .books
            .iter_mut()
            .filter_map(|(k, b)| {
                let before = b.active.len() + b.waiting.len();
                b.active.retain(|a| *a != agent);
                b.waiting.retain(|a| *a != agent);
                (b.active.len() + b.waiting.len() != before).then(|| k.clone())
            })
            .collect();
        let mut out = Vec::new();
        for sub in subs {
            out.extend(s.grant(&sub));
        }
        out
    }

    /// True while the work of `agent` waits for a stream.
    pub fn is_waiting(&self, agent: AgentId) -> bool {
        self.lock()
            .books
            .values()
            .any(|b| b.waiting.contains(&agent))
    }

    /// The streams of `sub`, when it has a limit.
    pub fn usage(&self, sub: &str) -> Option<StreamUsage> {
        let s = self.lock();
        let max = s.limits.get(sub).copied()?;
        let (in_use, waiting) = s
            .books
            .get(sub)
            .map_or((0, 0), |b| (b.active.len(), b.waiting.len()));
        Some(StreamUsage {
            max,
            in_use,
            waiting,
        })
    }
}

/// Read the limits of every subscription from the store into the gate,
/// and let the agents that may go now go (`Supervisor::stream_granted`).
/// Logs a failure; never fails.
pub async fn refresh(state: &crate::api::Shared) {
    let limits = state
        .db
        .read(|c| {
            Ok(crate::store::subscriptions::list(c)?
                .into_iter()
                .filter_map(|s| s.max_concurrent().map(|m| (s.name, m)))
                .collect::<HashMap<_, _>>())
        })
        .await;
    match limits {
        Ok(l) => {
            for id in state.usage.streams.set_limits(l) {
                state.supervisor.stream_granted(id);
            }
        }
        Err(e) => tracing::warn!("cannot read the stream limits: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(max: u32) -> StreamGate {
        let g = StreamGate::default();
        g.set_limits(HashMap::from([("gpu".to_owned(), max)]));
        g
    }

    /// Two slots, three agents: the third waits; the end of a turn gives
    /// its slot to the first that waits; the queue is FIFO across agents.
    #[test]
    fn two_slots_three_agents_fifo() {
        let g = gate(2);
        let (a, b, c, d) = (
            AgentId::new(),
            AgentId::new(),
            AgentId::new(),
            AgentId::new(),
        );
        let sub = Some("gpu");
        assert_eq!(g.take(sub, a), Take::Go);
        assert_eq!(g.take(sub, b), Take::Go);
        assert_eq!(g.take(sub, c), Take::Wait { new: true });
        assert_eq!(g.take(sub, c), Take::Wait { new: false });
        assert_eq!(g.take(sub, d), Take::Wait { new: true });
        assert!(g.is_waiting(c) && g.is_waiting(d));
        assert_eq!(
            g.usage("gpu"),
            Some(StreamUsage {
                max: 2,
                in_use: 2,
                waiting: 2
            })
        );
        // A holder goes on (a second turn into a busy agent).
        assert_eq!(g.take(sub, a), Take::Go);
        // a ends its turn: c (first in the queue) gets the slot, not d.
        assert_eq!(g.release(a), vec![c]);
        assert_eq!(g.take(sub, c), Take::Go);
        // a has more work: it waits behind d.
        assert_eq!(g.take(sub, a), Take::Wait { new: true });
        assert_eq!(g.release(b), vec![d]);
        assert_eq!(g.release(c), vec![a]);
        // A release of an agent with no slot frees nothing.
        assert!(g.release(c).is_empty());
        // Other subscriptions and no subscription have no limit.
        assert_eq!(g.take(Some("other"), b), Take::Go);
        assert_eq!(g.take(None, b), Take::Go);
        assert_eq!(g.usage("other"), None);
    }

    /// A stop or a crash frees the slot and leaves the queue; nothing
    /// deadlocks on an agent that is gone.
    #[test]
    fn stop_and_crash_free_the_slot() {
        let g = gate(1);
        let (a, b, c) = (AgentId::new(), AgentId::new(), AgentId::new());
        let sub = Some("gpu");
        assert_eq!(g.take(sub, a), Take::Go);
        assert!(matches!(g.take(sub, b), Take::Wait { .. }));
        assert!(matches!(g.take(sub, c), Take::Wait { .. }));
        // b stops while it waits: it leaves the queue.
        assert!(g.forget(b).is_empty());
        assert!(!g.is_waiting(b));
        // a crashes: c gets the slot.
        assert_eq!(g.forget(a), vec![c]);
        assert_eq!(g.usage("gpu").map(|u| (u.in_use, u.waiting)), Some((1, 0)));
    }

    /// A compaction counts as a stream: it takes a slot like a turn.
    /// A turn that the runner reports counts past the limit (a restart of
    /// the daemon recomputes from the turns that run).
    #[test]
    fn busy_agents_count_and_a_restart_recomputes() {
        let g = gate(1);
        let (a, b) = (AgentId::new(), AgentId::new());
        let sub = Some("gpu");
        g.note_busy(sub, a);
        g.note_busy(sub, b);
        assert_eq!(g.usage("gpu").map(|u| u.in_use), Some(2));
        // b ends: still one in use (a), so a new agent waits.
        assert!(g.release(b).is_empty());
        let c = AgentId::new();
        assert!(matches!(g.take(sub, c), Take::Wait { .. }));
        assert_eq!(g.release(a), vec![c]);
        // A compaction takes a free slot only, and never queues.
        let d = AgentId::new();
        assert!(!g.take_if_free(sub, d));
        assert!(!g.is_waiting(d));
        assert_eq!(g.release(c), Vec::<AgentId>::new());
        assert!(g.take_if_free(sub, d));
        assert!(matches!(g.take(sub, AgentId::new()), Take::Wait { .. }));
        // A fresh gate (a daemon restart) knows only what it sees.
        let fresh = gate(1);
        assert_eq!(fresh.usage("gpu").map(|u| u.in_use), Some(0));
        fresh.note_busy(sub, a);
        assert_eq!(fresh.usage("gpu").map(|u| u.in_use), Some(1));
    }

    /// A raised or removed limit lets the waiting agents go.
    #[test]
    fn a_new_limit_lets_waiters_go() {
        let g = gate(1);
        let (a, b, c) = (AgentId::new(), AgentId::new(), AgentId::new());
        let sub = Some("gpu");
        assert_eq!(g.take(sub, a), Take::Go);
        assert!(matches!(g.take(sub, b), Take::Wait { .. }));
        assert!(matches!(g.take(sub, c), Take::Wait { .. }));
        assert_eq!(
            g.set_limits(HashMap::from([("gpu".to_owned(), 2)])),
            vec![b]
        );
        assert_eq!(g.set_limits(HashMap::new()), vec![c]);
        assert_eq!(g.take(sub, AgentId::new()), Take::Go);
        assert!(!g.is_waiting(c));
    }
}
