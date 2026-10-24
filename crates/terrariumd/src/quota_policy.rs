//! The quota policy (PLAN §16.2, "Policy hooks").
//!
//! - When a window of a subscription passes its `alert_at` percent, the
//!   daemon mails the operator and emits a `quota.alert` event (the
//!   dashboard banner). Each window period alerts once.
//! - At 100 percent the daemon holds new turns of every agent on that
//!   subscription: [`Supervisor::hold_turns`] marks the agent `blocked`
//!   with the reset time, and turns wait. After the reset, the hold ends
//!   and the waiting turns go out.
//! - A project may set daily budgets on tokens or turns. The same alert
//!   and hold apply, with the next UTC midnight as the reset.
//!
//! A supervisor that cannot hold turns (`SupervisorError::NotReady`) still
//! gets the hold row and the events, with `enforced: false`, so the
//! dashboard and a sysadmin agent see the state.
//!
//! Project budgets come from the project file, because `ProjectDef` has no
//! field for them yet:
//!
//! ```toml
//! # projects/terrarium.toml
//! [[budget]]
//! subscription = "anthropic-max"   # optional: every subscription
//! tokens_per_day = 5_000_000       # input + output tokens, UTC day
//! turns_per_day = 200
//! alert_at = 80                    # optional, percent
//! ```

use std::path::Path;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use terrarium_core::AgentId;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::api::Shared;
use crate::db::Tx;
use crate::error::ApiResult;
use crate::events::EventDraft;
use crate::store::mail::{self, Address, Outgoing};
use crate::store::subscriptions as subs;
use crate::store::usage::{self as ustore, UsageFilter};
use crate::supervisor::{SupervisorError, TurnHold};

/// A daily budget of a project.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Budget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_day: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turns_per_day: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert_at: Option<u8>,
}

/// The budgets of `project` in `<config>/projects/*.toml`: `[[budget]]` or
/// one `[budget]` table. A file without `name` is named by its stem.
pub fn budgets(config_dir: &Path, project: &str) -> Vec<Budget> {
    let Ok(entries) = std::fs::read_dir(config_dir.join("projects")) else {
        return Vec::new();
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().is_none_or(|x| x != "toml") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v) = toml::from_str::<toml::Value>(&text) else {
            continue;
        };
        let name = v
            .get("name")
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
            .or_else(|| path.file_stem().map(|s| s.to_string_lossy().into_owned()));
        if name.as_deref() != Some(project) {
            continue;
        }
        return match v.get("budget") {
            Some(toml::Value::Array(a)) => a
                .iter()
                .filter_map(|b| b.clone().try_into::<Budget>().ok())
                .collect(),
            Some(t @ toml::Value::Table(_)) => {
                t.clone().try_into::<Budget>().ok().into_iter().collect()
            }
            _ => Vec::new(),
        };
    }
    Vec::new()
}

/// A window that has passed its reset counts as zero.
fn effective_pct(used: f64, resets_at: Option<&str>, now: DateTime<Utc>) -> f64 {
    let reset = resets_at
        .and_then(|r| DateTime::parse_from_rfc3339(r).ok())
        .map(|r| r.with_timezone(&Utc));
    match reset {
        Some(r) if r <= now => 0.0,
        _ => used,
    }
}

/// The dashboard page of the subscriptions, for a quota alert mail.
pub fn subscriptions_link(web: &crate::settings::WebConfig) -> String {
    web.dashboard_link("/subscriptions")
}

/// The dashboard page of a project's spend, for a budget alert mail.
pub fn spend_link(web: &crate::settings::WebConfig, project: &str) -> String {
    web.dashboard_link(&format!("/p/{project}/spend"))
}

/// Mail the operator from the daemon: the sender is `system`, and so is
/// the event actor.
fn mail_operator(t: &mut Tx<'_>, subject: &str, body: &str) -> ApiResult<()> {
    mail::send(
        t,
        Outgoing {
            from: &Address::System,
            to: &Address::Operator,
            subject,
            body,
            thread: None,
            in_reply_to: None,
        },
    )?;
    Ok(())
}

/// One alert to raise once per period.
struct Alert {
    scope: String,
    window: String,
    period: String,
    level: &'static str,
    subject: String,
    body: String,
    data: Value,
    project: Option<String>,
}

/// Raise the alerts that were not raised in their period yet.
async fn raise(state: &Shared, alerts: Vec<Alert>) -> ApiResult<()> {
    if alerts.is_empty() {
        return Ok(());
    }
    state
        .db
        .write("system", move |t| {
            for a in &alerts {
                if !ustore::mark_alert(t, &a.scope, &a.window, &a.period, a.level)? {
                    continue;
                }
                mail_operator(t, &a.subject, &a.body)?;
                let mut d = EventDraft::new("quota.alert", a.subject.clone(), a.data.clone());
                if let Some(p) = &a.project {
                    d = d.project(p.clone());
                }
                t.emit(d)?;
            }
            Ok(())
        })
        .await
}

/// Hold an agent's new turns for `cause`.
pub async fn hold(
    state: &Shared,
    agent: &str,
    cause: &str,
    until: Option<DateTime<Utc>>,
    reason: &str,
) -> ApiResult<()> {
    let key = agent.to_owned();
    let known = state.db.read(move |c| ustore::holds_of(c, &key)).await?;
    if known.iter().any(|h| h.cause == cause) {
        return Ok(());
    }
    let enforced = match agent.parse::<AgentId>() {
        Ok(id) => {
            let h = TurnHold {
                until,
                reason: reason.to_owned(),
            };
            match state.supervisor.hold_turns(id, Some(h)).await {
                Ok(()) => true,
                Err(SupervisorError::NotReady(m)) => {
                    warn!(agent, "quota hold is advice only: {m}");
                    false
                }
                Err(e) => {
                    warn!(agent, "quota hold failed: {e}");
                    false
                }
            }
        }
        Err(_) => false,
    };
    let (agent, cause, reason) = (agent.to_owned(), cause.to_owned(), reason.to_owned());
    let until_s = until.map(|u| u.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    state
        .db
        .write("system", move |t| {
            if !ustore::add_hold(t, &agent, &cause, until_s.as_deref(), &reason, enforced)? {
                return Ok(());
            }
            let a = crate::store::agents::get(t, &agent).ok();
            let mut d = EventDraft::new(
                "quota.blocked",
                format!(
                    "{}: turns held until {} ({reason})",
                    a.as_ref().map_or(agent.as_str(), |a| a.agent.name.as_str()),
                    until_s.as_deref().unwrap_or("further notice")
                ),
                json!({"cause": cause, "until": until_s, "reason": reason, "enforced": enforced}),
            );
            if let Some(a) = a {
                d = d
                    .agent(agent.clone(), a.agent.name)
                    .project(a.agent.project);
            }
            t.emit(d)?;
            Ok(())
        })
        .await
}

/// End a hold. When the agent has no other hold, its turns go out again.
pub async fn release(state: &Shared, agent: &str, cause: &str) -> ApiResult<()> {
    let (a, c) = (agent.to_owned(), cause.to_owned());
    let left = state
        .db
        .write("system", move |t| {
            if !ustore::remove_hold(t, &a, &c)? {
                return Ok(None);
            }
            let left = ustore::holds_of(t, &a)?;
            let view = crate::store::agents::get(t, &a).ok();
            let mut d = EventDraft::new(
                "quota.resumed",
                format!(
                    "{}: hold `{c}` ended",
                    view.as_ref().map_or(a.as_str(), |v| v.agent.name.as_str())
                ),
                json!({"cause": c, "holds_left": left.len()}),
            );
            if let Some(v) = view {
                d = d.agent(a.clone(), v.agent.name).project(v.agent.project);
            }
            t.emit(d)?;
            Ok(Some(left.len()))
        })
        .await?;
    if left == Some(0)
        && let Ok(id) = agent.parse::<AgentId>()
    {
        match state.supervisor.hold_turns(id, None).await {
            Ok(()) | Err(SupervisorError::NotReady(_)) => {}
            Err(e) => warn!(agent, "cannot release the quota hold: {e}"),
        }
    }
    Ok(())
}

/// Apply the policy to the latest windows of one subscription.
pub async fn evaluate_subscription(state: &Shared, name: &str) -> ApiResult<()> {
    let key = name.to_owned();
    let (sub, windows, on) = state
        .db
        .read(move |c| {
            Ok((
                subs::get(c, &key)?,
                subs::latest_windows(c, &key)?,
                ustore::agents_on(c, &key)?,
            ))
        })
        .await?;
    let now = Utc::now();
    let mut alerts = Vec::new();
    let mut exhausted_until: Option<Option<DateTime<Utc>>> = None;
    for w in &windows {
        let pct = effective_pct(w.used_pct, w.resets_at.as_deref(), now);
        let period = w.resets_at.clone().unwrap_or_else(ustore::today_start);
        let reset_txt = w.resets_at.as_deref().unwrap_or("an unknown time");
        if pct >= f64::from(sub.alert_at) {
            alerts.push(Alert {
                scope: sub.name.clone(),
                window: w.window.clone(),
                period: period.clone(),
                level: "alert",
                subject: format!(
                    "Quota: {} {} window at {pct:.0}%",
                    sub.name, w.window
                ),
                body: format!(
                    "Subscription `{}` ({}) has used {pct:.0}% of its {} window. The alert level is {}%.\nThe window resets at {reset_txt}.\n\nMove agents to another subscription with `terrarium admin agent set <name> --subscription <other>`, or wait for the reset.\n\nOpen: {}\n",
                    sub.name, sub.provider, w.window, sub.alert_at, subscriptions_link(&state.web)
                ),
                data: json!({"subscription": sub.name, "window": w.window, "used_pct": pct,
                             "resets_at": w.resets_at, "level": "alert", "alert_at": sub.alert_at}),
                project: None,
            });
        }
        if pct >= 100.0 {
            let reset = w
                .resets_at
                .as_deref()
                .and_then(|r| DateTime::parse_from_rfc3339(r).ok())
                .map(|r| r.with_timezone(&Utc));
            exhausted_until = Some(match (exhausted_until.flatten(), reset) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            });
            alerts.push(Alert {
                scope: sub.name.clone(),
                window: w.window.clone(),
                period,
                level: "exhausted",
                subject: format!("Quota: {} {} window is used up", sub.name, w.window),
                body: format!(
                    "Subscription `{}` has used all of its {} window. New turns of its {} agent(s) wait until {reset_txt}.\n\nOpen: {}\n",
                    sub.name, w.window, on.len(), subscriptions_link(&state.web)
                ),
                data: json!({"subscription": sub.name, "window": w.window, "used_pct": pct,
                             "resets_at": w.resets_at, "level": "exhausted"}),
                project: None,
            });
        }
    }
    raise(state, alerts).await?;
    let cause = format!("quota:{}", sub.name);
    match exhausted_until {
        Some(until) => {
            let reason = format!(
                "subscription `{}` is at its limit; resets at {}",
                sub.name,
                until.map_or_else(|| "an unknown time".into(), |u| u.to_rfc3339())
            );
            for (id, _, _) in &on {
                hold(state, id, &cause, until, &reason).await?;
            }
        }
        None => {
            let holds = state.db.read(ustore::holds).await?;
            for h in holds.iter().filter(|h| h.cause == cause) {
                release(state, &h.agent, &cause).await?;
            }
        }
    }
    Ok(())
}

fn next_midnight(now: DateTime<Utc>) -> DateTime<Utc> {
    let day = now.date_naive() + ChronoDuration::days(1);
    day.and_hms_opt(0, 0, 0)
        .map(|t| t.and_utc())
        .unwrap_or(now + ChronoDuration::days(1))
}

/// Apply the daily budgets of a project.
pub async fn check_budgets(state: &Shared, project: &str) -> ApiResult<()> {
    let list = budgets(&state.config_dir, project);
    if list.is_empty() {
        return Ok(());
    }
    let now = Utc::now();
    let today = ustore::today_start();
    for b in list {
        let filter = UsageFilter {
            project: Some(project.to_owned()),
            subscription: b.subscription.clone(),
            since: Some(today.clone()),
            ..Default::default()
        };
        let totals = state.db.read(move |c| ustore::totals(c, &filter)).await?;
        let scope = match &b.subscription {
            Some(s) => format!("project:{project}/{s}"),
            None => format!("project:{project}"),
        };
        let alert_at = f64::from(b.alert_at.unwrap_or(80));
        let mut over = false;
        let mut alerts = Vec::new();
        for (window, used, limit) in [
            ("tokens_per_day", totals.tokens(), b.tokens_per_day),
            ("turns_per_day", totals.turns, b.turns_per_day),
        ] {
            let Some(limit) = limit.filter(|l| *l > 0) else {
                continue;
            };
            #[allow(clippy::cast_precision_loss)]
            let pct = used as f64 / limit as f64 * 100.0;
            for (level, min) in [("alert", alert_at), ("exhausted", 100.0)] {
                if pct < min {
                    continue;
                }
                alerts.push(Alert {
                    scope: scope.clone(),
                    window: window.into(),
                    period: today.clone(),
                    level,
                    subject: format!("Budget: {scope} {window} at {pct:.0}%"),
                    body: format!(
                        "Project `{project}` has used {used} of its {limit} {window} budget ({pct:.0}%).{}\n\nOpen: {}\n",
                        if level == "exhausted" {
                            " New turns wait until the next UTC day."
                        } else {
                            ""
                        },
                        spend_link(&state.web, project)
                    ),
                    data: json!({"project": project, "subscription": b.subscription, "window": window,
                                 "used": used, "limit": limit, "used_pct": pct, "level": level}),
                    project: Some(project.to_owned()),
                });
            }
            over |= pct >= 100.0;
        }
        raise(state, alerts).await?;
        if over {
            let (p, s) = (project.to_owned(), b.subscription.clone());
            let agents = state
                .db
                .read(move |c| ustore::agents_of(c, &p, s.as_deref()))
                .await?;
            let until = next_midnight(now);
            let reason = format!("the daily budget of `{scope}` is used up");
            for (id, _, _) in &agents {
                hold(state, id, &format!("budget:{scope}"), Some(until), &reason).await?;
            }
        }
    }
    Ok(())
}

/// End the holds whose reset time has passed.
pub async fn release_expired(state: &Shared) -> ApiResult<usize> {
    let now = Utc::now();
    let holds = state.db.read(ustore::holds).await?;
    let mut n = 0;
    for h in holds {
        let passed = h
            .until
            .as_deref()
            .and_then(|u| DateTime::parse_from_rfc3339(u).ok())
            .is_some_and(|u| u.with_timezone(&Utc) <= now);
        if passed {
            release(state, &h.agent, &h.cause).await?;
            n += 1;
        }
    }
    Ok(n)
}

/// Give the stored holds to the supervisor again after a daemon start.
pub async fn reapply(state: &Shared) -> ApiResult<()> {
    let holds = state.db.read(ustore::holds).await?;
    for h in holds {
        if let Ok(id) = h.agent.parse::<AgentId>() {
            let hold = TurnHold {
                until: h
                    .until
                    .as_deref()
                    .and_then(|u| DateTime::parse_from_rfc3339(u).ok())
                    .map(|u| u.with_timezone(&Utc)),
                reason: h.reason.clone(),
            };
            if let Err(e) = state.supervisor.hold_turns(id, Some(hold)).await {
                info!(agent = %h.agent, "stored quota hold not enforced: {e}");
            }
        }
    }
    Ok(())
}

/// The policy timer: release expired holds on each probe tick.
pub fn spawn(state: Shared) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = reapply(&state).await {
            warn!("cannot apply stored quota holds: {e}");
        }
        loop {
            tokio::time::sleep(state.usage.config.probe_tick).await;
            match release_expired(&state).await {
                Ok(0) => {}
                Ok(n) => info!(holds = n, "quota holds ended after their reset"),
                Err(e) => warn!("cannot release expired quota holds: {e}"),
            }
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn budgets_read_both_forms() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("projects");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("terrarium.toml"),
            "[[budget]]\nsubscription = \"s\"\ntokens_per_day = 100\n[[budget]]\nturns_per_day = 3\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("other.toml"),
            "name = \"ops\"\n[budget]\ntokens_per_day = 5\nalert_at = 50\n",
        )
        .unwrap();
        let b = budgets(tmp.path(), "terrarium");
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].subscription.as_deref(), Some("s"));
        assert_eq!(b[1].turns_per_day, Some(3));
        let o = budgets(tmp.path(), "ops");
        assert_eq!(o[0].alert_at, Some(50));
        assert!(budgets(tmp.path(), "none").is_empty());
    }

    #[test]
    fn passed_resets_count_as_zero() {
        let now = Utc::now();
        let past = (now - ChronoDuration::minutes(1)).to_rfc3339();
        let future = (now + ChronoDuration::minutes(1)).to_rfc3339();
        assert_eq!(effective_pct(100.0, Some(&past), now), 0.0);
        assert_eq!(effective_pct(100.0, Some(&future), now), 100.0);
        assert_eq!(effective_pct(90.0, None, now), 90.0);
        let m = next_midnight(now);
        assert!(m > now && m - now <= ChronoDuration::days(1));
    }
}
