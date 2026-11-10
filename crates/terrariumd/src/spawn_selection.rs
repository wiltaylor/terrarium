//! Admission by headroom for new ephemeral agents, not a live quota hold.

use rusqlite::Connection;
use terrarium_core::config::AgentDef;

use crate::api::Shared;
use crate::error::ApiResult;
use crate::store::subscriptions as subs;

/// Pick the account with most headroom below 80 percent in every window.
/// Missing samples are not evidence that an account is full. A cloud
/// subscription counts only when the definition names it as
/// `subscription`, never from the pool.
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
        let why = if let Some(why) = cloud_refusal(&sub, def.subscription.as_deref()) {
            Some(why)
        } else if !sub.enabled {
            Some("disabled")
        } else if !sub.harnesses.contains(&def.harness)
            || subs::plan_refusal(def.harness, &sub).is_some()
        {
            Some("incompatible harness")
        } else if !sub.is_selfhosted() && (!usable || !login) {
            Some("no usable login or API key")
        } else if state.usage.limits.limited(name, now) {
            Some("active usage limit")
        } else if state
            .usage
            .streams
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

/// A cloud subscription (`terrarium_core::cloud`) is billed per token: it
/// is a candidate only as the named `subscription` of the definition,
/// never as a pool member.
fn cloud_refusal(sub: &subs::Subscription, named: Option<&str>) -> Option<&'static str> {
    (sub.cloud_kind().is_some() && named != Some(sub.name.as_str()))
        .then_some("cloud subscription, used only when named")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_cloud_subscription_is_a_candidate_only_when_named() {
        let mut s: subs::Subscription = serde_json::from_value(serde_json::json!({
            "name": "aws", "provider": "bedrock", "harnesses": ["claude"], "alert_at": 80,
            "source": "api", "login_status": "api_key", "probe_status": "none",
            "created_at": "", "updated_at": ""
        }))
        .unwrap();
        assert_eq!(cloud_refusal(&s, Some("aws")), None);
        assert!(cloud_refusal(&s, None).is_some());
        assert!(cloud_refusal(&s, Some("max")).is_some());
        s.provider = "anthropic".into();
        assert_eq!(cloud_refusal(&s, None), None);
    }
}
