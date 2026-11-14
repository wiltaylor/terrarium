//! The service request sweep (TER-1264, design D3 and Q2 A): every minute,
//! the requests that did not start within their wait limit end as
//! `timed_out` and their callers get a mail; the payloads of ended
//! requests older than the 7-day retention are deleted; then the rows of
//! ended requests that ended more than [`ENV_REQUEST_KEEP`] ago (default
//! 30 days), with their payload gone, are deleted (TER-1285).

use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::api::Shared;
use crate::error::ApiResult;
use crate::store::mail::{self, Address, Outgoing};
use crate::store::services::{self as store, RequestRow};

/// How often the sweep runs.
pub const TICK: Duration = Duration::from_secs(60);

/// The global time that the row of an ended request stays after its end
/// (TER-1285). A duration of at least the payload retention.
pub const ENV_REQUEST_KEEP: &str = "TERRARIUM_SERVICE_REQUEST_KEEP";

/// The default of [`ENV_REQUEST_KEEP`].
pub const DEFAULT_REQUEST_KEEP: Duration = Duration::from_secs(30 * 24 * 3600);

/// The longest value of [`ENV_REQUEST_KEEP`]: 3650 days.
pub const MAX_REQUEST_KEEP: Duration = Duration::from_secs(3650 * 24 * 3600);

/// A value of [`ENV_REQUEST_KEEP`]: a duration of at least the payload
/// retention, so that a row goes only after its payload, and at most
/// [`MAX_REQUEST_KEEP`], so that the cutoff is always a valid time.
pub fn parse_request_keep(s: &str) -> Result<Duration, String> {
    let d = crate::settings::parse_duration(s)?;
    if d < terrarium_core::service::RETENTION {
        return Err(format!(
            "`{}`: keep the request rows at least 7d (the payload retention)",
            s.trim()
        ));
    }
    if d > MAX_REQUEST_KEEP {
        return Err(format!(
            "`{}`: keep the request rows at most 3650d",
            s.trim()
        ));
    }
    Ok(d)
}

/// The keep time from [`ENV_REQUEST_KEEP`]. Unset or empty: the default.
/// A value that does not parse gives the default, with a warning.
pub fn request_keep_from_env() -> Duration {
    match std::env::var(ENV_REQUEST_KEEP) {
        Ok(v) if !v.trim().is_empty() => parse_request_keep(&v).unwrap_or_else(|e| {
            warn!("{ENV_REQUEST_KEEP}: {e}; the request rows stay 30d");
            DEFAULT_REQUEST_KEEP
        }),
        _ => DEFAULT_REQUEST_KEEP,
    }
}

pub fn spawn(state: Shared) -> JoinHandle<()> {
    let keep = request_keep_from_env();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(TICK).await;
            if let Err(e) = sweep_with(&state, chrono::Utc::now(), keep).await {
                warn!("service sweep failed: {}", e.message);
            }
        }
    })
}

fn fmt(at: chrono::DateTime<chrono::Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// One sweep at `at` with the default keep time: `(timed out, payloads
/// deleted)`.
pub async fn sweep(state: &Shared, at: chrono::DateTime<chrono::Utc>) -> ApiResult<(usize, usize)> {
    sweep_with(state, at, DEFAULT_REQUEST_KEEP).await
}

/// One sweep at `at` that keeps the ended request rows for `keep` after
/// their end: `(timed out, payloads deleted)`.
pub async fn sweep_with(
    state: &Shared,
    at: chrono::DateTime<chrono::Utc>,
    keep: Duration,
) -> ApiResult<(usize, usize)> {
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
    // The rows go after their payload: a row whose payload is still there
    // stays (TER-1285).
    // A keep time with no valid cutoff skips the prune; the sweep goes on.
    let Some(cutoff) = chrono::Duration::from_std(keep)
        .ok()
        .and_then(|k| at.checked_sub_signed(k))
    else {
        warn!("the service request keep time is out of range: no prune");
        return Ok((timed_out.len(), deleted));
    };
    let cutoff = fmt(cutoff);
    let pruned = state
        .db
        .write("system", move |t| {
            store::prune_ended(t, &cutoff, store::PRUNE_BATCH)
        })
        .await?;
    if pruned > 0 {
        info!(rows = pruned, "pruned ended service requests");
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_request_keep_is_at_least_the_payload_retention() {
        assert_eq!(parse_request_keep("30d").unwrap(), DEFAULT_REQUEST_KEEP);
        assert_eq!(
            parse_request_keep("7d").unwrap(),
            terrarium_core::service::RETENTION
        );
        assert!(parse_request_keep("6d").unwrap_err().contains("7d"));
        assert_eq!(parse_request_keep("3650d").unwrap(), MAX_REQUEST_KEEP);
        assert!(parse_request_keep("3651d").unwrap_err().contains("3650d"));
        assert!(parse_request_keep("100000000d").is_err());
        assert!(parse_request_keep("soon").is_err());
    }
}
