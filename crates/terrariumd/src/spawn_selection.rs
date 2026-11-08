//! Admission by headroom for new ephemeral agents, not a live quota hold.

use rusqlite::Connection;
use terrarium_core::config::AgentDef;

use crate::error::ApiResult;
use crate::store::subscriptions as subs;
use crate::stream_limit::StreamGate;

/// Pick the account with most headroom below 80 percent in every window.
/// Missing samples are not evidence that an account is full.
pub fn pick(
    conn: &Connection,
    def: &AgentDef,
    streams: &StreamGate,
) -> ApiResult<Option<String>> {
    let now = chrono::Utc::now();
    let mut best = None;
    let mut lowest = f64::INFINITY;
    for name in def.subscription.iter().chain(&def.subscription_pool) {
        let Some(sub) = subs::find(conn, name)? else {
            continue;
        };
        if !sub.enabled
            || !sub.harnesses.contains(&def.harness)
            || subs::plan_refusal(def.harness, &sub).is_some()
            || matches!(sub.login_status.as_str(), subs::EXPIRED | subs::MISSING)
        {
            continue;
        }
        if streams
            .usage(name)
            .is_some_and(|u| u.in_use >= u.max as usize)
        {
            continue;
        }
        let used = subs::latest_windows(conn, name)?.iter().map(|w| {
            let expired = w
                .resets_at
                .as_deref()
                .and_then(|r| chrono::DateTime::parse_from_rfc3339(r).ok())
                .is_some_and(|r| r <= now);
            if expired { 0.0 } else { w.used_pct }
        }).fold(0.0, f64::max);
        if used < 80.0 && used < lowest {
            best = Some(name.clone());
            lowest = used;
        }
    }
    Ok(best)
}
