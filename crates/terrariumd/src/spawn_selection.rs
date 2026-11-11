//! Admission by headroom for new ephemeral agents, not a live quota hold.

use rusqlite::Connection;
use terrarium_core::config::AgentDef;

use crate::api::Shared;
use crate::error::ApiResult;
use crate::store::subscriptions as subs;

/// Pick the account with most headroom below 80 percent in every window.
/// Missing samples are not evidence that an account is full. A cloud
/// subscription counts only when the definition names it as
/// `subscription`, never from the pool, and never when `fallback` is true
/// (the `<template>-alt` of the automatic main/alt switch). A cloud
/// subscription has room up to 100 percent of its budget window: the
/// operator's line is the alert at 80 and the hold at 100.
pub fn pick(
    conn: &Connection,
    def: &AgentDef,
    state: &Shared,
    fallback: bool,
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
        let why = if let Some(why) = cloud_refusal(&sub, def.subscription.as_deref(), fallback) {
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
        // A cloud subscription: room up to its used-up budget.
        let line = if sub.cloud_kind().is_some() {
            100.0
        } else {
            80.0
        };
        if used < line && used < lowest {
            best = Some(name.clone());
            lowest = used;
        }
        if used >= line {
            refusals.push(format!("{name}: window use {used}% is at or above {line}%"));
        }
    }
    Ok(best)
}

/// A cloud subscription (`terrarium_core::cloud`) is billed per token: it
/// is a candidate only as the named `subscription` of the definition the
/// caller asked for, never as a pool member and never on the `-alt`
/// fallback (`fallback`).
fn cloud_refusal(
    sub: &subs::Subscription,
    named: Option<&str>,
    fallback: bool,
) -> Option<&'static str> {
    if sub.provider == "litellm" {
        return (fallback || named != Some(sub.name.as_str()))
            .then_some("LiteLLM subscription, used only when named, never as automatic fallback");
    }
    sub.cloud_kind()?;
    if fallback {
        return Some("cloud subscription, never taken by the automatic -alt fallback");
    }
    (named != Some(sub.name.as_str())).then_some("cloud subscription, used only when named")
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
        assert_eq!(cloud_refusal(&s, Some("aws"), false), None);
        assert!(cloud_refusal(&s, None, false).is_some());
        assert!(cloud_refusal(&s, Some("max"), false).is_some());
        // The `-alt` fallback never takes it, even when the alt names it.
        assert!(
            cloud_refusal(&s, Some("aws"), true)
                .unwrap()
                .contains("-alt")
        );
        s.provider = "anthropic".into();
        assert_eq!(cloud_refusal(&s, None, false), None);
        assert_eq!(cloud_refusal(&s, None, true), None);
    }
}
