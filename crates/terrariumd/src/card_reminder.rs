//! The card reminder (PLAN §18, "Card reminder"; GitHub issue #20): an
//! agent that goes `idle` after a turn while it is the assignee of cards
//! in the working column gets one `[System:card]` turn that asks it to
//! update them (a comment, a move to `review` or `blocked`, or a new ETA).
//!
//! - **When.** The lifecycle listener (`crate::lifecycle`) sees the
//!   `idle` status of an agent that was `working` since its last start.
//!   `waiting`, `blocked`, `stopped`, `error`, and an agent that only
//!   started (a wake with no turn yet) get nothing. A paused agent, an
//!   agent that compacts, and an agent with a pending reset get nothing.
//!   The check runs at the change to `idle`, so it comes before the idle
//!   stop, which needs a full rest.
//! - **Limits.** Once for each card: a card is due again only after
//!   [`REMIND_AGAIN`], or when its ETA passed after the last reminder. The
//!   agent's own answer (a comment, an ETA) does not make the card due
//!   again, so a reminder cannot loop. The reminder turn makes the agent
//!   `working` and then `idle` again; that idle finds no due card. The
//!   times are in memory: a daemon restart forgets them.
//! - **Setting.** `card_reminder` of the agent; `null` is the global
//!   default ([`ENV_CARD_REMINDER`], on).
//! - **History.** Each reminded card gets a `card` event with the change
//!   `reminded` (`crate::store::board::note_reminder`).

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use terrarium_core::turn_tag::{SystemKind, TurnSource, render};
use terrarium_core::{AgentId, AgentStatus};
use tracing::{debug, info, warn};

use crate::api::Shared;
use crate::store::{agents, board};

/// The global default of `card_reminder` (`on` or `off`). Unset: on.
pub const ENV_CARD_REMINDER: &str = "TERRARIUM_CARD_REMINDER";

/// After a reminder, the time until the same card is due again.
pub const REMIND_AGAIN: Duration = Duration::from_secs(60 * 60);

/// The global default from [`ENV_CARD_REMINDER`]. Unset or empty: on. A
/// value that does not parse gives on, with a warning.
pub fn default_from_env() -> bool {
    match std::env::var(ENV_CARD_REMINDER) {
        Ok(v) if !v.trim().is_empty() => crate::settings::parse_switch(&v).unwrap_or_else(|e| {
            warn!("{ENV_CARD_REMINDER}: {e}; the card reminder is on");
            true
        }),
        _ => true,
    }
}

/// The last reminder of one card for one agent.
#[derive(Debug, Clone, Copy)]
struct Sent {
    at: Instant,
    /// The ETA had passed at that reminder.
    eta_passed: bool,
}

/// The state of the card reminder (`Lifecycle::reminders`).
#[derive(Debug)]
pub struct Reminders {
    default_on: AtomicBool,
    /// The agents that were `working` since their last start or idle.
    ran: Mutex<HashSet<AgentId>>,
    /// The agents whose [`check`] runs: the idle stop waits for it.
    checking: Mutex<HashSet<AgentId>>,
    sent: Mutex<HashMap<(AgentId, String), Sent>>,
}

impl Default for Reminders {
    fn default() -> Self {
        Self {
            default_on: AtomicBool::new(default_from_env()),
            ran: Mutex::default(),
            checking: Mutex::default(),
            sent: Mutex::default(),
        }
    }
}

fn guard<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Reminders {
    /// The `card_reminder` of an agent that sets none.
    pub fn default_on(&self) -> bool {
        self.default_on.load(Ordering::Relaxed)
    }

    /// Change the global default. For tests.
    pub fn set_default(&self, on: bool) {
        self.default_on.store(on, Ordering::Relaxed);
    }

    /// A status of `agent`. True when it is `idle` after a turn: then the
    /// agent counts as checking ([`Reminders::checking`]) until the caller's
    /// [`check`] ends.
    pub fn note_status(&self, agent: AgentId, status: AgentStatus) -> bool {
        let mut ran = guard(&self.ran);
        match status {
            AgentStatus::Working => {
                ran.insert(agent);
                false
            }
            AgentStatus::Idle => {
                let due = ran.remove(&agent);
                if due {
                    guard(&self.checking).insert(agent);
                }
                due
            }
            AgentStatus::Waiting | AgentStatus::Blocked => false,
            AgentStatus::Starting
            | AgentStatus::Stopped
            | AgentStatus::Error
            | AgentStatus::Restarting => {
                ran.remove(&agent);
                false
            }
        }
    }

    /// True while the [`check`] of `agent` runs. The idle stop waits: the
    /// reminder comes first.
    pub fn checking(&self, agent: AgentId) -> bool {
        guard(&self.checking).contains(&agent)
    }

    /// The cards of `cards` (`(id, eta passed)`) that are due at `now`.
    fn due(&self, agent: AgentId, cards: &[(String, bool)], now: Instant) -> Vec<(String, bool)> {
        let sent = guard(&self.sent);
        cards
            .iter()
            .filter(|(id, eta_passed)| match sent.get(&(agent, id.clone())) {
                None => true,
                Some(s) => {
                    now.saturating_duration_since(s.at) >= REMIND_AGAIN
                        || (*eta_passed && !s.eta_passed)
                }
            })
            .cloned()
            .collect()
    }

    fn note_sent(&self, agent: AgentId, cards: &[(String, bool)], now: Instant) {
        let mut sent = guard(&self.sent);
        for (id, eta_passed) in cards {
            sent.insert(
                (agent, id.clone()),
                Sent {
                    at: now,
                    eta_passed: *eta_passed,
                },
            );
        }
    }
}

/// True when the RFC 3339 time `eta` is before `now`.
fn passed(eta: Option<&str>, now: chrono::DateTime<chrono::Utc>) -> bool {
    eta.and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .is_some_and(|t| t < now)
}

/// The text of the reminder turn (without the tag). `cards`: `(id, ETA,
/// ETA passed)`.
pub fn message(cards: &[(String, Option<String>, bool)]) -> String {
    let ids: Vec<&str> = cards.iter().map(|(id, _, _)| id.as_str()).collect();
    let (noun, verb, them) = if ids.len() == 1 {
        ("Card", "is", "it")
    } else {
        ("Cards", "are", "them")
    };
    let mut text = format!(
        "{noun} {} {verb} still in progress and assigned to you, and you are idle. Update \
         {them} before you go idle: comment, move to review or blocked, or set a new ETA. If \
         you wait on something, register a wait.",
        ids.join(", ")
    );
    let late: Vec<String> = cards
        .iter()
        .filter(|(_, _, p)| *p)
        .map(|(id, eta, _)| format!("{id} (ETA {})", eta.as_deref().unwrap_or("?")))
        .collect();
    if !late.is_empty() {
        text.push_str(&format!(" The ETA has passed: {}.", late.join(", ")));
    }
    text
}

/// Remind `agent` of its due cards in the working column, when it is
/// still `idle` and the reminder is on for it. Ends the
/// [`Reminders::checking`] mark of [`Reminders::note_status`].
pub async fn check(state: &Shared, agent: AgentId) {
    remind(state, agent).await;
    guard(&state.lifecycle.reminders.checking).remove(&agent);
}

async fn remind(state: &Shared, agent: AgentId) {
    if state.lifecycle.compacting(agent) {
        return;
    }
    let key = agent.to_string();
    let read = state
        .db
        .read(move |c| {
            let v = agents::get(c, &key)?;
            let reset = agents::pending_reset(c, &key)?.is_some();
            let cards = board::working_cards_of(c, &key)?;
            Ok((v, reset, cards))
        })
        .await;
    let (view, reset, cards) = match read {
        Ok(r) => r,
        Err(e) => {
            debug!(agent = %agent, "the card reminder cannot read the agent: {e}");
            return;
        }
    };
    let reminders = &state.lifecycle.reminders;
    let on = view
        .agent
        .card_reminder
        .unwrap_or_else(|| reminders.default_on());
    if !on || view.paused || reset || cards.is_empty() {
        return;
    }
    if state.supervisor.status(agent).await != Some(AgentStatus::Idle) {
        return;
    }
    let now = Instant::now();
    let wall = chrono::Utc::now();
    let marked: Vec<(String, bool)> = cards
        .iter()
        .map(|(id, eta)| (id.clone(), passed(eta.as_deref(), wall)))
        .collect();
    let due = reminders.due(agent, &marked, now);
    if due.is_empty() {
        return;
    }
    let listed: Vec<(String, Option<String>, bool)> = due
        .iter()
        .map(|(id, p)| {
            let eta = cards
                .iter()
                .find(|(c, _)| c == id)
                .and_then(|(_, e)| e.clone());
            (id.clone(), eta, *p)
        })
        .collect();
    let text = render(&TurnSource::system(SystemKind::Card), &message(&listed));
    if let Err(e) = state.supervisor.send_turn(agent, text).await {
        debug!(agent = %agent, "the card reminder did not go out: {e}");
        return;
    }
    reminders.note_sent(agent, &due, now);
    info!(agent = %agent, cards = ?due.iter().map(|(c, _)| c).collect::<Vec<_>>(), "card reminder");
    let written = state
        .db
        .write("system", move |t| {
            for (id, p) in &due {
                board::note_reminder(t, id, *p)?;
            }
            Ok(())
        })
        .await;
    if let Err(e) = written {
        warn!(agent = %agent, "cannot record the card reminder: {e}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn a() -> AgentId {
        "01K8AGENT0000000000000000A".parse().unwrap()
    }

    /// Idle counts only after `working`: a start that goes idle, and a
    /// second idle with no turn between, do not.
    #[test]
    fn idle_counts_after_a_turn_only() {
        let r = Reminders::default();
        assert!(!r.note_status(a(), AgentStatus::Starting));
        assert!(!r.note_status(a(), AgentStatus::Idle));
        assert!(!r.note_status(a(), AgentStatus::Working));
        assert!(r.note_status(a(), AgentStatus::Idle));
        assert!(!r.note_status(a(), AgentStatus::Idle));
        // Waiting keeps the turn: a later idle still counts.
        r.note_status(a(), AgentStatus::Working);
        assert!(!r.note_status(a(), AgentStatus::Waiting));
        assert!(r.note_status(a(), AgentStatus::Idle));
        // A stop forgets it.
        r.note_status(a(), AgentStatus::Working);
        r.note_status(a(), AgentStatus::Stopped);
        assert!(!r.note_status(a(), AgentStatus::Idle));
    }

    /// A card is due once; again after [`REMIND_AGAIN`], or when its ETA
    /// passed after the last reminder.
    #[test]
    fn a_card_is_due_again_after_the_wait_or_a_passed_eta() {
        let r = Reminders::default();
        let t0 = Instant::now();
        let cards = vec![("P-1".to_owned(), false), ("P-2".to_owned(), false)];
        assert_eq!(r.due(a(), &cards, t0), cards);
        r.note_sent(a(), &cards, t0);
        assert!(r.due(a(), &cards, t0 + REMIND_AGAIN / 2).is_empty());
        let late = vec![("P-1".to_owned(), true), ("P-2".to_owned(), false)];
        assert_eq!(
            r.due(a(), &late, t0 + REMIND_AGAIN / 2),
            vec![("P-1".to_owned(), true)]
        );
        r.note_sent(a(), &late[..1], t0 + REMIND_AGAIN / 2);
        assert!(r.due(a(), &late, t0 + REMIND_AGAIN / 2).is_empty());
        assert_eq!(r.due(a(), &late, t0 + REMIND_AGAIN), vec![late[1].clone()]);
        // A new card is due at once.
        let more = vec![("P-3".to_owned(), false)];
        assert_eq!(r.due(a(), &more, t0), more);
    }

    #[test]
    fn the_message_lists_the_cards_and_the_passed_etas() {
        let one = message(&[("P-1".into(), None, false)]);
        assert!(one.starts_with("Card P-1 is still in progress"), "{one}");
        assert!(!one.contains("ETA has passed"), "{one}");
        let two = message(&[
            ("P-1".into(), Some("2026-01-01T00:00:00.000Z".into()), true),
            ("P-2".into(), None, false),
        ]);
        assert!(
            two.starts_with("Cards P-1, P-2 are still in progress"),
            "{two}"
        );
        assert!(
            two.ends_with("The ETA has passed: P-1 (ETA 2026-01-01T00:00:00.000Z)."),
            "{two}"
        );
    }

    #[test]
    fn an_eta_in_the_past_has_passed() {
        let now = chrono::Utc::now();
        assert!(passed(Some("2020-01-01T00:00:00.000Z"), now));
        assert!(!passed(Some("2999-01-01T00:00:00.000Z"), now));
        assert!(!passed(None, now));
        assert!(!passed(Some("soon"), now));
    }
}
