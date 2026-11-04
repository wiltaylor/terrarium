//! The calendar engine (docs/api.md §33): the recurrence rules
//! ([`recur`]) and the notice tick.
//!
//! The scheduler task (`crate::scheduler`) calls [`tick`] on its timer.
//! One tick is one write: it finds the occurrences that started or ended
//! since the tick before, mails the assignees, emits
//! `calendar.event_started` and `calendar.event_ended`, and keeps a row
//! for each occurrence and edge (`crate::store::calendar::tick`). The
//! rows and the stored time of the last tick make a daemon restart safe:
//! no notice goes out twice, a late start notice goes out only for an
//! occurrence that has not ended, and an occurrence that started and
//! ended while the daemon was down gets no mail.

pub mod recur;

use chrono::{DateTime, Utc};
use tracing::warn;

use crate::api::Shared;
use crate::store::calendar::{self, TickReport};

/// Run the notice tick at `now`. Tests pass a fake clock.
pub async fn tick(state: &Shared, now: DateTime<Utc>) -> TickReport {
    match state
        .db
        .write("system", move |t| calendar::tick(t, now))
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("the calendar tick failed: {e}");
            TickReport::default()
        }
    }
}

/// Drop the old notice rows (the daily prune).
pub async fn prune(state: &Shared) {
    let r = state
        .db
        .write("system", |t| calendar::prune(t, Utc::now()))
        .await;
    if let Err(e) = r {
        warn!("the calendar prune failed: {e}");
    }
}
