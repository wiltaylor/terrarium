//! Admission by headroom for new ephemeral agents, not a live quota hold.

use rusqlite::Connection;
use terrarium_core::config::AgentDef;

use crate::error::ApiResult;
use crate::store::subscriptions as subs;
use crate::api::Shared;

/// Pick the account with most headroom below 80 percent in every window.
/// Missing samples are not evidence that an account is full.
pub fn pick(
    conn: &Connection,
    def: &AgentDef,
    state: &Shared,
    refusals: &mut Vec<String>,
) -> ApiResult<Option<String>> {
    let now = chrono::Utc::now();
    let mut best = None;
    let mut lowest = f64::INFINITY;
    for name in def.subscription.iter().chain(&def.subscription_pool) {
        let Some(sub) = subs::find(conn, name)? else {
            refusals.push(format!("{name}: subscription missing"));
            continue;
        };
        let usable = matches!(
            sub.login_status.as_str(),
            subs::LOGGED_IN | subs::EXPIRING | "unknown" | subs::API_KEY
        );
        let login = sub.is_selfhosted()
            || crate::accounts::has_credential(&state.state_dir, name, def.harness)
            || sub.env_var.is_some();
        let why = if !sub.enabled {
            Some("disabled")
        } else if !sub.harnesses.contains(&def.harness)
            || subs::plan_refusal(def.harness, &sub).is_some()
        {
            Some("incompatible harness")
        } else if !sub.is_selfhosted() && (!usable || !login) {
            Some("no usable login or API key")
        } else if state.usage.limits.limited(name, now) {
            Some("active usage limit")
        } else if state.usage.streams
            .usage(name)
            .is_some_and(|u| u.in_use >= u.max as usize)
        {
            Some("concurrency limit reached")
        } else {
            None
        };
        if let Some(why) = why {
            refusals.push(format!("{name}: {why}"));
            continue;
        }
        let used = subs::latest_windows(conn, name)?
            .iter()
            .map(|w| crate::quota_policy::effective_pct(w.used_pct, w.resets_at.as_deref(), now))
            .fold(0.0, f64::max);
        if used < 80.0 && used < lowest {
            best = Some(name.clone());
            lowest = used;
        }
        if used >= 80.0 {
            refusals.push(format!("{name}: window use {used}% is at or above 80%"));
        }
    }
    Ok(best)
}
