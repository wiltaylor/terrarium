//! The service request sweep (TER-1264, design D3 and Q2 A): every minute,
//! the requests that did not start within their wait limit end as
//! `timed_out`; each ended request whose result mail is due gets it
//! (TER-1268); the payloads of requests that ended more than the 7-day
//! retention ago are deleted.

use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::api::Shared;
use crate::error::ApiResult;
use crate::store::services as store;

/// How often the sweep runs.
pub const TICK: Duration = Duration::from_secs(60);

pub fn spawn(state: Shared) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(TICK).await;
            if let Err(e) = sweep(&state, chrono::Utc::now()).await {
                warn!("service sweep failed: {}", e.message);
            }
        }
    })
}

fn fmt(at: chrono::DateTime<chrono::Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// One sweep at `at`: `(timed out, payloads deleted)`.
pub async fn sweep(state: &Shared, at: chrono::DateTime<chrono::Utc>) -> ApiResult<(usize, usize)> {
    let now = fmt(at);
    let timed_out = state
        .db
        .write("system", move |t| {
            let rows = store::time_out_waits(t, &now)?;
            for r in &rows {
                t.audit(
                    "service.timed_out",
                    &r.service,
                    r.project.as_deref(),
                    serde_json::json!({"request": r.id, "wait_until": r.wait_until}),
                )?;
            }
            Ok(rows)
        })
        .await?;
    for r in &timed_out {
        info!(request = %r.id, service = %r.service, "service request timed out waiting");
    }
    // The result mail of each request that ended (here, in a run, or by a
    // cancel) and has none yet (TER-1268, `crate::service_result`).
    if let Err(e) = crate::service_result::send_due(state).await {
        warn!("cannot send the service result mails: {}", e.message);
    }
    let retention = chrono::Duration::from_std(terrarium_core::service::RETENTION)
        .unwrap_or_else(|_| chrono::Duration::days(7));
    let cutoff = fmt(at - retention);
    let expired = state
        .db
        .read(move |c| store::expired_payloads(c, &cutoff))
        .await?;
    let mut deleted = 0;
    for id in expired {
        let dir = store::payload_dir(&state.state_dir, &id);
        match tokio::fs::remove_dir_all(&dir).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                warn!(path = %dir.display(), "cannot delete a service payload: {e}");
                continue;
            }
        }
        state
            .db
            .write("system", move |t| store::mark_payload_deleted(t, &id))
            .await?;
        deleted += 1;
    }
    Ok((timed_out.len(), deleted))
}
