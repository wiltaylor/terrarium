//! The usage-limit block (PLAN §16.2, §16.3): the refusal of the provider
//! stops an agent, not a probe window.
//!
//! - An adapter that knows the usage-limit error of its harness turns it
//!   into `Event::UsageLimit` (`docs/compat.md`, "Usage limits"). Only
//!   Claude Code has a known signal now: the `StopFailure` hook with
//!   `error` `rate_limit` or `billing_error`. The other harnesses have none
//!   yet; their errors stay notices.
//! - The listener ([`spawn`]) acts on each `usage_limit` event
//!   ([`on_limit`]). The subscription of the agent goes in the
//!   [`LimitBook`] until its reset. An agent with an account pool moves to
//!   the next member that did not hit a limit
//!   ([`crate::account_swap::auto_swap`]; `TERRARIUM_AUTO_SWAP=off` turns
//!   it off). Else the daemon holds the new turns of the agent
//!   ([`crate::quota_policy::hold`], cause `limit:<subscription>`): the
//!   agent is `blocked` with the message of the provider, and a
//!   `quota.blocked` event goes out. The operator gets one mail for each
//!   block (an open item of the Inbox).
//! - The hold ends:
//!   - at the reset time, when the message gives one (the policy timer,
//!     [`crate::quota_policy::release_expired`]);
//!   - when a later turn of the agent ends with no error (the harness
//!     waited for the reset by itself);
//!   - when the agent moves to another account (a swap); or
//!   - when the operator starts or restarts the agent (a retry).
//!
//! The book is in memory: after a daemon start, every member of a pool is
//! a candidate again. The hold rows stay in the store.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::api::Shared;
use crate::error::ApiResult;
use crate::events::StoredEvent;
use crate::store::agents;
use crate::store::mail::{self, Address, Outgoing};
use crate::store::usage as ustore;

/// The cause prefix of a usage-limit hold.
pub const CAUSE_PREFIX: &str = "limit:";

/// The hold cause for a usage limit of `subscription`.
pub fn cause(subscription: &str) -> String {
    format!("{CAUSE_PREFIX}{subscription}")
}

/// The subscriptions that hit a usage limit, and when each limit resets
/// (`None`: not known).
#[derive(Debug, Default)]
pub struct LimitBook(Mutex<HashMap<String, Option<DateTime<Utc>>>>);

impl LimitBook {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Option<DateTime<Utc>>>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Record a limit of `subscription` that resets at `until`.
    pub fn mark(&self, subscription: &str, until: Option<DateTime<Utc>>) {
        self.lock().insert(subscription.to_owned(), until);
    }

    /// True while the limit of `subscription` has not reset.
    pub fn limited(&self, subscription: &str, now: DateTime<Utc>) -> bool {
        self.lock()
            .get(subscription)
            .is_some_and(|until| until.is_none_or(|u| u > now))
    }

    /// The subscription serves turns again.
    pub fn clear(&self, subscription: &str) {
        self.lock().remove(subscription);
    }
}

/// What [`on_limit`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The agent moved to this member of its pool.
    Swapped(String),
    /// The turns of the agent wait. `new` is false when the agent had this
    /// hold already (no new mail).
    Held { cause: String, new: bool },
}

/// Act on a usage-limit error of agent `agent`: swap it to the next member
/// of its pool, else hold its turns and mail the operator.
pub async fn on_limit(
    state: &Shared,
    agent: &str,
    message: &str,
    resets_at: Option<DateTime<Utc>>,
) -> ApiResult<Outcome> {
    let key = agent.to_owned();
    let view = state.db.read(move |c| agents::get(c, &key)).await?;
    let now = Utc::now();
    // A reset time in the past says nothing: the limit is there now.
    let resets_at = resets_at.filter(|r| *r > now);
    let sub = view.agent.subscription.clone();
    let limits = &state.usage.limits;
    if let Some(s) = &sub {
        limits.mark(s, resets_at);
    }
    let reason = match &sub {
        Some(s) => format!("usage limit on `{s}`: {message}"),
        None => format!("usage limit: {message}"),
    };
    if let Some(o) = crate::account_swap::auto_swap(state, view.clone(), reason.clone(), |n| {
        limits.limited(n, now)
    })
    .await?
    {
        return Ok(Outcome::Swapped(o.to));
    }
    let cause = cause(
        &sub.clone()
            .unwrap_or_else(|| view.agent.harness.to_string()),
    );
    let new = crate::quota_policy::hold(state, agent, &cause, resets_at, &reason).await?;
    if new {
        let link = state
            .web
            .dashboard_link(&format!("/agents/{}", view.agent.id));
        let (subject, body) = mail_text(&view, sub.as_deref(), message, resets_at, &link);
        state
            .db
            .write("system", move |t| {
                mail::send(
                    t,
                    Outgoing {
                        from: &Address::System,
                        to: &Address::Operator,
                        subject: &subject,
                        body: &body,
                        thread: None,
                        in_reply_to: None,
                    },
                )?;
                Ok(())
            })
            .await?;
    }
    Ok(Outcome::Held { cause, new })
}

/// The subject and the body of the block mail.
fn mail_text(
    view: &agents::AgentView,
    sub: Option<&str>,
    message: &str,
    resets_at: Option<DateTime<Utc>>,
    link: &str,
) -> (String, String) {
    let a = &view.agent;
    let name = format!("{}/{}", a.project, a.name);
    let on = sub.map_or_else(String::new, |s| format!(" on subscription `{s}`"));
    let wait = match resets_at {
        Some(r) => format!(
            "New turns of the agent wait until the reset at {}.",
            r.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        ),
        None => "The provider gives no reset time that Terrarium can read. New turns of the \
                 agent wait until a turn ends with no error, a swap, or a start or restart \
                 of the agent."
            .to_owned(),
    };
    let pool = if a.subscription_pool.len() >= 2 {
        "No other account of its pool is free."
    } else {
        "Give the agent an account pool for the auto-swap."
    };
    (
        format!("Usage limit: {name} is blocked"),
        format!(
            "Agent `{name}` ({}) hit a usage limit{on}.\nThe provider said: {message}\n{wait}\n\
             {pool} Move the agent to another account with `terrarium admin agent swap {} \
             <subscription>`.\n\nOpen: {link}\n",
            a.harness, a.name
        ),
    )
}

/// End every usage-limit hold of `agent`. Returns how many ended.
pub async fn release_all(state: &Shared, agent: &str) -> ApiResult<usize> {
    let key = agent.to_owned();
    let holds = state.db.read(move |c| ustore::holds_of(c, &key)).await?;
    let mut n = 0;
    for h in holds.iter().filter(|h| h.cause.starts_with(CAUSE_PREFIX)) {
        crate::quota_policy::release(state, agent, &h.cause).await?;
        n += 1;
    }
    Ok(n)
}

/// The state of the listener for each agent with a usage-limit hold: true
/// when a turn started (or a tool ran) after the hold.
#[derive(Debug, Default)]
struct Listener {
    progress: HashMap<String, bool>,
}

impl Listener {
    async fn on_event(&mut self, state: &Shared, e: &StoredEvent) {
        let Some(agent) = e.agent_id.as_deref() else {
            return;
        };
        match e.kind.as_str() {
            "usage_limit" => {
                // The data of the event is the `Event::UsageLimit`.
                let message = e.data["message"].as_str().unwrap_or("no message");
                let resets_at = e.data["resets_at"]
                    .as_str()
                    .and_then(|r| DateTime::parse_from_rfc3339(r).ok())
                    .map(|r| r.with_timezone(&Utc));
                match on_limit(state, agent, message, resets_at).await {
                    Ok(Outcome::Swapped(to)) => {
                        info!(agent, to = %to, "usage limit: the agent moved to another account");
                        self.progress.remove(agent);
                    }
                    Ok(Outcome::Held { .. }) => {
                        self.progress.insert(agent.to_owned(), false);
                    }
                    Err(e) => warn!(agent, "usage limit: cannot act on the error: {e}"),
                }
            }
            "turn_start" | "tool_use" | "tool_finished" => {
                if let Some(p) = self.progress.get_mut(agent) {
                    *p = true;
                }
            }
            "turn_end" if self.progress.get(agent) == Some(&true) => {
                self.progress.remove(agent);
                let key = agent.to_owned();
                if let Ok(v) = state.db.read(move |c| agents::get(c, &key)).await
                    && let Some(s) = &v.agent.subscription
                {
                    state.usage.limits.clear(s);
                }
                match release_all(state, agent).await {
                    Ok(0) => {}
                    Ok(n) => info!(agent, holds = n, "usage limit: a turn ended with no error"),
                    Err(e) => warn!(agent, "usage limit: cannot release the hold: {e}"),
                }
            }
            _ => {}
        }
    }
}

/// Start the listener. The stored usage-limit holds count as holds with no
/// turn after them.
pub fn spawn(state: Shared) -> JoinHandle<()> {
    let mut rx = state.db.hub().subscribe();
    tokio::spawn(async move {
        let mut l = Listener::default();
        match state.db.read(ustore::holds).await {
            Ok(holds) => {
                for h in holds.iter().filter(|h| h.cause.starts_with(CAUSE_PREFIX)) {
                    l.progress.insert(h.agent.clone(), false);
                }
            }
            Err(e) => warn!("usage limit: cannot read the stored holds: {e}"),
        }
        loop {
            match rx.recv().await {
                Ok(e) => l.on_event(&state, &e).await,
                Err(RecvError::Lagged(n)) => warn!("the usage-limit listener missed {n} events"),
                Err(RecvError::Closed) => return,
            }
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_book_forgets_a_limit_after_its_reset() {
        let b = LimitBook::default();
        let now = Utc::now();
        b.mark("a", Some(now + chrono::Duration::minutes(5)));
        b.mark("b", None);
        b.mark("c", Some(now - chrono::Duration::minutes(5)));
        assert!(b.limited("a", now));
        assert!(!b.limited("a", now + chrono::Duration::minutes(6)));
        assert!(b.limited("b", now));
        assert!(!b.limited("c", now));
        assert!(!b.limited("d", now));
        b.clear("b");
        assert!(!b.limited("b", now));
    }
}
