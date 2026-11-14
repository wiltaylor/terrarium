//! The service request sweep (TER-1264, design D3 and Q2 A): every minute,
//! the requests that did not start within their wait limit end as
//! `timed_out` and their callers get a mail; the payloads of ended
//! requests older than the 7-day retention are deleted.

use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::api::Shared;
use crate::error::ApiResult;
use crate::store::mail::{self, Address, Outgoing};
use crate::store::services::{self as store, RequestRow};

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
        // One mail per request, each in its own write: a caller that is
        // gone does not hold back the others.
        let r = r.clone();
        if let Err(e) = state
            .db
            .write("system", move |t| mail_timed_out(t, &r))
            .await
        {
            warn!(
                "cannot mail the time-out of a service request: {}",
                e.message
            );
        }
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

/// The caller's address of `r`: the operator, or the agent of its label.
fn caller_address(r: &RequestRow) -> Option<Address> {
    let Some(id) = &r.caller_agent else {
        return Some(Address::Operator);
    };
    let (project, name) = r.caller.strip_prefix("agent:")?.split_once('/')?;
    Some(Address::Agent {
        id: id.clone(),
        name: name.to_owned(),
        project: project.to_owned(),
    })
}

fn mail_timed_out(t: &mut crate::db::Tx<'_>, r: &RequestRow) -> ApiResult<()> {
    let Some(to) = caller_address(r) else {
        return Ok(());
    };
    let subject = format!("Service {}: request {} timed out", r.service, r.id);
    let body = format!(
        "Your request `{}` to service `{}` did not start before its wait limit ({}), so it \
         ended as `timed_out`. It was not run.\n\nSend it again with `terrarium service call {}` \
         when the service has room. See it with `terrarium service status {}`.",
        r.id, r.service, r.wait_until, r.service, r.id
    );
    mail::send(
        t,
        Outgoing {
            from: &Address::System,
            to: &to,
            subject: &subject,
            body: &body,
            thread: None,
            in_reply_to: None,
        },
    )?;
    Ok(())
}
