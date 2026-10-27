//! Move an agent to another plan account (PLAN §16.3): the operator's
//! `POST /api/agents/{id}/subscription` and the auto-swap of the quota
//! policy.
//!
//! The rules:
//!
//! - The new subscription exists, the agent's harness can spend it, and
//!   it has the provider of the current one. When the agent has a pool,
//!   the new subscription is a member.
//! - When the current subscription has a plan account, the new one must
//!   have a login of the harness too (`crate::accounts`).
//! - The supervisor applies it ([`Supervisor::swap_subscription`]): live
//!   for Claude Code and pi, a restart with resume for Codex and Copilot.
//! - The store then names the new subscription, an `agent.swap` audit row
//!   records the old and the new one and how it applied, and an
//!   `agent.swapped` event goes out. The quota hold of the old
//!   subscription ends for the agent.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;

use crate::api::Shared;
use crate::error::{ApiError, ApiResult};
use crate::events::EventDraft;
use crate::store::agents::{self, AgentView};
use crate::store::subscriptions::{self as subs, Subscription};
use crate::supervisor::SwapApplied;

/// The answer of a swap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SwapOutcome {
    pub agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub to: String,
    /// `live`, `restart_scheduled`, `restarted`, or `next_start`.
    pub applied: SwapApplied,
}

/// Why a swap is not allowed, or `Ok`.
pub fn check(
    state_dir: &std::path::Path,
    view: &AgentView,
    from: Option<&Subscription>,
    to: &Subscription,
) -> ApiResult<()> {
    let a = &view.agent;
    if a.subscription.as_deref() == Some(to.name.as_str()) {
        return Err(ApiError::conflict(format!(
            "agent `{}` is on subscription `{}` already",
            a.name, to.name
        )));
    }
    if !to.harnesses.contains(&a.harness) {
        return Err(ApiError::invalid(format!(
            "the {} harness cannot spend subscription `{}`",
            a.harness, to.name
        )));
    }
    if !a.subscription_pool.is_empty() && !a.subscription_pool.contains(&to.name) {
        return Err(ApiError::invalid(format!(
            "subscription `{}` is not in the pool of agent `{}` ({}); add it to \
             `subscription_pool` first",
            to.name,
            a.name,
            a.subscription_pool.join(", ")
        )));
    }
    if let Some(f) = from {
        if f.provider != to.provider {
            return Err(ApiError::invalid(format!(
                "subscription `{}` is {}, but the agent's subscription `{}` is {}; a swap \
                 stays with one provider",
                to.name, to.provider, f.name, f.provider
            )));
        }
        let on_account = crate::accounts::has_credential(state_dir, &f.name, a.harness);
        if on_account && !crate::accounts::has_credential(state_dir, &to.name, a.harness) {
            return Err(ApiError::invalid(format!(
                "subscription `{}` has no {} login; log it in with the Add-subscription \
                 wizard first",
                to.name, a.harness
            )));
        }
    }
    Ok(())
}

/// Swap the agent of `view` to subscription `to`. `actor` is the audit
/// actor; `auto` is the reason of an auto-swap (`None`: an operator or an
/// agent asked).
pub async fn swap(
    state: &Shared,
    actor: String,
    view: AgentView,
    to: &str,
    auto: Option<String>,
) -> ApiResult<SwapOutcome> {
    let (from_name, to_name) = (view.agent.subscription.clone(), to.to_owned());
    let (from, target) = {
        let (f, t) = (from_name.clone(), to_name.clone());
        state
            .db
            .read(move |c| {
                let from = match f {
                    Some(n) => subs::find(c, &n)?,
                    None => None,
                };
                Ok((from, subs::get(c, &t)?))
            })
            .await?
    };
    check(&state.state_dir, &view, from.as_ref(), &target)?;
    let mut agent = view.agent.clone();
    agent.subscription = Some(to_name.clone());
    let applied = state
        .supervisor
        .swap_subscription(agent)
        .await
        .map_err(crate::api::agents::supervisor_error)?;
    let id = view.agent.id.to_string();
    let (project, name) = (view.agent.project.clone(), view.agent.name.clone());
    let (f2, t2, reason) = (from_name.clone(), to_name.clone(), auto.clone());
    state
        .db
        .write(actor, move |t| {
            agents::set_subscription(t, &id, &t2)?;
            let detail = json!({"from": f2, "to": t2, "applied": applied, "auto": reason.is_some(),
                                "reason": reason});
            t.audit("agent.swap", &id, Some(&project), detail.clone())?;
            let summary = format!(
                "{name}: subscription {} → {t2} ({})",
                f2.as_deref().unwrap_or("none"),
                applied.as_str()
            );
            t.emit(
                EventDraft::new("agent.swapped", summary, detail)
                    .agent(id.clone(), name.clone())
                    .project(project.clone()),
            )?;
            Ok(())
        })
        .await?;
    if let Some(f) = &from_name {
        crate::quota_policy::release(state, &view.agent.id.to_string(), &format!("quota:{f}"))
            .await?;
    }
    Ok(SwapOutcome {
        agent: view.agent.id.to_string(),
        from: from_name,
        to: to_name,
        applied,
    })
}

/// A window of a subscription at or past 100 percent, with a reset in the
/// future (or no reset time).
fn full(windows: &[subs::QuotaSample], now: DateTime<Utc>) -> bool {
    windows.iter().any(|w| {
        let reset = w
            .resets_at
            .as_deref()
            .and_then(|r| DateTime::parse_from_rfc3339(r).ok())
            .map(|r| r.with_timezone(&Utc));
        w.used_pct >= 100.0 && reset.is_none_or(|r| r > now)
    })
}

/// The pool member that an agent on the full subscription `current` moves
/// to: the next member after `current` in pool order (then from the
/// start) that is not full, is not expired or missing, and has a login of
/// the harness (or a stored key). `None`: every member is full.
pub fn pick(
    state_dir: &std::path::Path,
    view: &AgentView,
    members: &[(Subscription, Vec<subs::QuotaSample>)],
    now: DateTime<Utc>,
) -> Option<String> {
    let a = &view.agent;
    let current = a.subscription.as_deref()?;
    let pool = &a.subscription_pool;
    let at = pool.iter().position(|p| p == current).unwrap_or(0);
    let order = pool.iter().skip(at + 1).chain(pool.iter().take(at));
    for name in order {
        if name == current {
            continue;
        }
        let Some((s, windows)) = members.iter().find(|(s, _)| &s.name == name) else {
            continue;
        };
        let usable = matches!(
            s.login_status.as_str(),
            subs::LOGGED_IN | subs::EXPIRING | "unknown" | subs::API_KEY
        );
        let login =
            crate::accounts::has_credential(state_dir, name, a.harness) || s.env_var.is_some();
        if usable && login && s.harnesses.contains(&a.harness) && !full(windows, now) {
            return Some(name.clone());
        }
    }
    None
}

/// The auto-swap of the quota policy for the agents on the full
/// subscription `sub` (PLAN §16.3). Each agent with a pool moves to the
/// next member that is not full. Returns the agents that stay (no pool,
/// auto-swap off, or every member full): the hold applies to them.
pub async fn auto_swap(
    state: &Shared,
    sub: &str,
    on: Vec<(String, String, String)>,
) -> ApiResult<Vec<(String, String, String)>> {
    if !state.usage.config.auto_swap {
        return Ok(on);
    }
    let mut stay = Vec::new();
    let now = Utc::now();
    for (id, name, project) in on {
        let key = id.clone();
        let view = match state.db.read(move |c| agents::get(c, &key)).await {
            Ok(v) => v,
            Err(_) => {
                stay.push((id, name, project));
                continue;
            }
        };
        if view.agent.subscription_pool.len() < 2 {
            stay.push((id, name, project));
            continue;
        }
        let pool = view.agent.subscription_pool.clone();
        let members = state
            .db
            .read(move |c| {
                let mut out = Vec::new();
                for n in &pool {
                    if let Some(s) = subs::find(c, n)? {
                        let w = subs::latest_windows(c, n)?;
                        out.push((s, w));
                    }
                }
                Ok(out)
            })
            .await?;
        let Some(to) = pick(&state.state_dir, &view, &members, now) else {
            stay.push((id, name, project));
            continue;
        };
        let reason = format!("subscription `{sub}` is at its limit");
        match swap(state, "system".into(), view, &to, Some(reason)).await {
            Ok(o) => tracing::info!(
                agent = %name,
                from = sub,
                to = %o.to,
                applied = o.applied.as_str(),
                "auto-swap: the agent moved to another account of its pool"
            ),
            Err(e) => {
                tracing::warn!(agent = %name, "auto-swap failed: {}", e.message);
                stay.push((id, name, project));
            }
        }
    }
    Ok(stay)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use terrarium_core::HarnessId;

    fn sub(name: &str, status: &str) -> Subscription {
        serde_json::from_value(json!({
            "name": name, "provider": "anthropic", "harnesses": ["claude"], "alert_at": 80,
            "source": "api", "login_status": status, "probe_status": "ok",
            "created_at": "", "updated_at": ""
        }))
        .unwrap()
    }

    fn sample(pct: f64, resets: Option<String>) -> subs::QuotaSample {
        serde_json::from_value(json!({
            "id": 1, "subscription": "x", "window": "5h", "used_pct": pct,
            "resets_at": resets, "raw": {}, "at": ""
        }))
        .unwrap()
    }

    fn view(current: &str, pool: &[&str]) -> AgentView {
        let mut v: AgentView = serde_json::from_value(json!({
            "id": "01K8AGENT0000000000000000A", "name": "a1", "project": "p", "harness": "claude",
            "workspace": "/w", "source": "api", "created_at": "", "updated_at": ""
        }))
        .unwrap();
        v.agent.subscription = Some(current.into());
        v.agent.subscription_pool = pool.iter().map(|s| (*s).to_owned()).collect();
        v
    }

    /// The auto-swap picks the next pool member that has a login and room,
    /// in pool order after the current one; none when all are full.
    #[test]
    fn pick_takes_the_next_member_with_room() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path();
        let home = tmp.path().join("home");
        for s in ["a", "b", "c"] {
            crate::accounts::write_credential(state, s, HarnessId::Claude, b"{}", &home).unwrap();
        }
        let now = Utc::now();
        let later = Some((now + chrono::Duration::hours(1)).to_rfc3339());
        let past = Some((now - chrono::Duration::hours(1)).to_rfc3339());
        let members = vec![
            (sub("a", "logged_in"), vec![sample(100.0, later.clone())]),
            (sub("b", "logged_in"), vec![sample(100.0, later.clone())]),
            (sub("c", "logged_in"), vec![sample(40.0, later.clone())]),
            (sub("d", "logged_in"), vec![]),
        ];
        let v = view("a", &["a", "b", "c", "d"]);
        assert_eq!(pick(state, &v, &members, now).as_deref(), Some("c"));
        // From `c`: `d` has no login, so `a` (its window reset passed).
        let mut m2 = members.clone();
        m2[0].1 = vec![sample(100.0, past)];
        let v = view("c", &["a", "b", "c", "d"]);
        assert_eq!(pick(state, &v, &m2, now).as_deref(), Some("a"));
        // Every member full: no swap, the hold applies.
        let mut m3 = members.clone();
        m3[2].1 = vec![sample(100.0, later)];
        let v = view("a", &["a", "b", "c"]);
        assert_eq!(pick(state, &v, &m3, now), None);
        // An expired member is skipped.
        let mut m4 = members;
        m4[2].0.login_status = "expired".into();
        assert_eq!(pick(state, &v, &m4, now), None);
    }

    #[test]
    fn check_refuses_a_member_outside_the_pool_and_a_missing_login() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path();
        let home = tmp.path().join("home");
        crate::accounts::write_credential(state, "a", HarnessId::Claude, b"{}", &home).unwrap();
        let v = view("a", &["a", "b"]);
        let (a, b, c) = (
            sub("a", "logged_in"),
            sub("b", "logged_in"),
            sub("c", "logged_in"),
        );
        let e = check(state, &v, Some(&a), &c).unwrap_err();
        assert!(e.message.contains("not in the pool"), "{}", e.message);
        let e = check(state, &v, Some(&a), &b).unwrap_err();
        assert!(e.message.contains("no claude login"), "{}", e.message);
        crate::accounts::write_credential(state, "b", HarnessId::Claude, b"{}", &home).unwrap();
        check(state, &v, Some(&a), &b).unwrap();
        assert!(check(state, &v, Some(&a), &a).is_err());
        let mut other = sub("b", "logged_in");
        other.provider = "zai".into();
        assert!(check(state, &v, Some(&a), &other).is_err());
    }
}
