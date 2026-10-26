//! The schedule timer (docs/api.md §19). Every [`TICK`] it finds the due
//! schedules, moves each one's `next_run_at` on, and starts its run. At a
//! daemon start, [`startup`] ends the runs that the old daemon left
//! `running` and applies `catch_up` to the runs it missed while it was
//! down. Both take the time as an argument, so tests drive them with a
//! fake clock.

use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::api::Shared;
use crate::error::ApiResult;
use crate::store::schedules::{self, Run};
use terrarium_core::schedule::CatchUp;

/// The period of the timer.
pub const TICK: Duration = Duration::from_secs(10);

/// One started run.
pub type Started = JoinHandle<ApiResult<Run>>;

fn start(state: &Shared, id: String, trigger: &'static str, run_as: String) -> Started {
    let st = state.clone();
    tokio::spawn(async move {
        let r = crate::api::run_schedule(&st, &id, trigger, run_as).await;
        if let Err(e) = &r {
            warn!(schedule = %id, "a schedule run failed to record: {e}");
        }
        r
    })
}

/// Start the runs of the schedules due at `now`. Each due schedule's
/// `next_run_at` moves past `now` first, in one write, so the next tick
/// does not start it again. Returns the started runs.
pub async fn tick(state: &Shared, now: DateTime<Utc>) -> Vec<Started> {
    let due = state
        .db
        .write("system", move |t| {
            let due = schedules::due(t, now)?;
            for s in &due {
                schedules::advance(t, &s.id, now)?;
            }
            Ok(due)
        })
        .await;
    match due {
        Ok(due) => due
            .into_iter()
            .map(|s| start(state, s.id, "cron", s.run_as))
            .collect(),
        Err(e) => {
            warn!("the schedule tick cannot read the due schedules: {e}");
            Vec::new()
        }
    }
}

/// At a daemon start: fail the runs left `running`, then, for each
/// enabled schedule whose next run passed more than one tick ago (a run
/// missed while the daemon was down), compute the next run from `now`.
/// With `catch_up = once`, start one run with the trigger `catch_up`.
pub async fn startup(state: &Shared, now: DateTime<Utc>) -> Vec<Started> {
    let before = now - chrono::Duration::from_std(TICK).unwrap_or_default();
    let missed = state
        .db
        .write("system", move |t| {
            let stale = schedules::fail_stale_runs(t, now)?;
            if stale > 0 {
                info!(runs = stale, "schedule runs ended with the old daemon");
            }
            let missed = schedules::missed(t, before)?;
            for s in &missed {
                schedules::advance(t, &s.id, now)?;
            }
            Ok(missed)
        })
        .await;
    match missed {
        Ok(missed) => missed
            .into_iter()
            .filter(|s| s.catch_up == CatchUp::Once)
            .map(|s| start(state, s.id, "catch_up", s.run_as))
            .collect(),
        Err(e) => {
            warn!("the schedule catch-up failed: {e}");
            Vec::new()
        }
    }
}

/// The timer: [`startup`], then [`tick`] every [`TICK`].
pub fn spawn(state: Shared) -> JoinHandle<()> {
    tokio::spawn(async move {
        drop(startup(&state, Utc::now()).await);
        loop {
            tokio::time::sleep(TICK).await;
            drop(tick(&state, Utc::now()).await);
        }
    })
}
