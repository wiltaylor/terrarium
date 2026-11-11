//! The monthly USD budget of a cloud subscription (`terrarium_core::cloud`,
//! TER-630).
//!
//! The spend of the UTC month is the sum of the `costUSD` running totals of
//! the Claude Code sessions on the subscription ([`spend`]). Each check
//! writes the `budget` window to `quota_samples`, like the OpenRouter
//! credits window: `used_pct`, the next month's start as `resets_at`, and
//! `raw` with `used_usd`, `remaining_usd`, `budget_usd`, `currency`, and
//! `month`. The quota notices ([`crate::quota_notices`]) mail the operator
//! at `alert_at` (default 80), 90, and 100 percent, once each per month.
//!
//! Budgets send mail only, including at 100 percent. The collector
//! checks after each new total, and the policy timer checks
//! every cloud subscription on each tick ([`check_all`]).

use chrono::{DateTime, Utc};
use serde_json::json;
use terrarium_core::cloud::{self, BudgetWindow};
use tracing::warn;

use crate::api::Shared;
use crate::db::Tx;
use crate::error::ApiResult;
use crate::store::cloud_spend;
use crate::store::subscriptions::{self as subs, Subscription};

/// Use local accounting only when LiteLLM has no fresh authoritative spend.
pub fn litellm_fallback(
    conn: &rusqlite::Connection,
    sub: &Subscription,
    now: DateTime<Utc>,
) -> ApiResult<bool> {
    if sub.provider != "litellm" {
        return Ok(false);
    }
    if sub.probe_status == "failed"
        && sub
            .probe_error
            .as_deref()
            .is_some_and(|e| e.contains("no spend from LiteLLM"))
    {
        return Ok(true);
    }
    Ok(!subs::latest_windows(conn, &sub.name)?.iter().any(|w| {
        w.window == "key-spend"
            && DateTime::parse_from_rfc3339(&w.at)
                .is_ok_and(|at| now.signed_duration_since(at) < chrono::Duration::minutes(10))
    }))
}

/// The spend of `sub` in the month of `now` against its budget. `None`
/// when it is not a cloud subscription or has no budget.
pub fn window(
    conn: &rusqlite::Connection,
    sub: &Subscription,
    now: DateTime<Utc>,
) -> ApiResult<Option<BudgetWindow>> {
    let budget = if sub.provider == "litellm" {
        if !litellm_fallback(conn, sub, now)? {
            return Ok(None);
        }
        sub.selfhosted.as_ref().and_then(|s| s.budget_usd)
    } else {
        sub.cloud_kind()
            .and(sub.cloud.as_ref())
            .and_then(|c| c.budget_usd)
    };
    let Some(budget) = budget else {
        return Ok(None);
    };
    let spent = cloud_spend::month_spend(conn, &sub.name, &cloud::month_key(now))?;
    Ok(Some(cloud::budget_window(budget, spent)))
}

/// Write the budget window of `sub` and send the quota notices of its
/// lines. Returns the window, `None` with no budget.
pub fn write_window(
    t: &mut Tx<'_>,
    sub: &Subscription,
    now: DateTime<Utc>,
    link: &str,
) -> ApiResult<Option<BudgetWindow>> {
    let Some(w) = window(t, sub, now)? else {
        return Ok(None);
    };
    let resets = cloud::next_month_start(now).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let raw = json!({
        "budget_usd": w.budget_usd,
        "used_usd": w.used_usd,
        "remaining_usd": w.remaining_usd,
        "currency": "USD",
        "month": cloud::month_key(now),
    });
    let latest = subs::latest_windows(t, &sub.name)?
        .into_iter()
        .find(|s| s.window == cloud::BUDGET_WINDOW);
    // A repeat of the same numbers writes no new sample.
    let same = latest.as_ref().is_some_and(|s| {
        s.used_pct == w.used_pct
            && s.resets_at.as_deref() == Some(resets.as_str())
            && s.raw.get("budget_usd") == raw.get("budget_usd")
            && s.raw.get("used_usd") == raw.get("used_usd")
    });
    if !same {
        subs::insert_sample(
            t,
            &sub.name,
            cloud::BUDGET_WINDOW,
            w.used_pct,
            Some(&resets),
            &raw,
        )?;
    }
    crate::quota_notices::check(
        t,
        sub,
        cloud::BUDGET_WINDOW,
        w.used_pct,
        Some(&resets),
        link,
        now,
    )?;
    Ok(Some(w))
}

/// Check the budget of subscription `name`: the window and the notices.
pub async fn check(state: &Shared, name: &str) -> ApiResult<()> {
    let now = Utc::now();
    let link = crate::quota_policy::subscriptions_link(&state.web);
    let key = name.to_owned();
    state
        .db
        .write("system", move |t| {
            let Some(sub) = subs::find(t, &key)? else {
                return Ok(());
            };
            if sub.cloud_kind().is_none() && sub.provider != "litellm" {
                return Ok(());
            }
            write_window(t, &sub, now, &link)?;
            Ok(())
        })
        .await
}

/// Check every cloud subscription ([`check`]), after the prune of the
/// spend months that are too old ([`cloud_spend::prune`]). Errors are
/// logged.
pub async fn check_all(state: &Shared) {
    let names = match state.db.read(subs::list).await {
        Ok(all) => all
            .into_iter()
            .filter(|s| s.cloud_kind().is_some() || s.provider == "litellm")
            .map(|s| s.name)
            .collect::<Vec<_>>(),
        Err(e) => {
            warn!("cloud budget: cannot list the subscriptions: {e}");
            return;
        }
    };
    // Most installations have no cloud subscriptions. Avoid a write and
    // a scan of the spend table on every policy tick in that case.
    if names.is_empty() {
        return;
    }
    let oldest = cloud_spend::oldest_kept(Utc::now());
    match state
        .db
        .write("system", move |t| cloud_spend::prune(t, &oldest))
        .await
    {
        Ok(0) => {}
        Ok(n) => tracing::info!(rows = n, "cloud budget: pruned old months of spend"),
        Err(e) => warn!("cloud budget: cannot prune old spend: {e}"),
    }
    for n in names {
        if let Err(e) = check(state, &n).await {
            warn!(subscription = %n, "cloud budget: check failed: {e}");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::events::EventHub;
    use crate::store::projects::SOURCE_API;
    use rusqlite::Connection;
    use terrarium_core::config::SubscriptionDef;

    fn db() -> Db {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut conn).unwrap();
        Db::new(conn, EventHub::default())
    }

    fn def(name: &str, provider: &str, budget: Option<u32>) -> SubscriptionDef {
        SubscriptionDef {
            name: name.into(),
            provider: provider.into(),
            plan: None,
            harnesses: vec![terrarium_core::HarnessId::Claude],
            probe: None,
            alert_at: 80,
            selfhosted: None,
            cloud: (provider != "anthropic").then(|| cloud::CloudSettings {
                region: Some("us-east-1".into()),
                budget_usd: budget,
                ..Default::default()
            }),
        }
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[tokio::test]
    async fn litellm_fallback_yields_to_fresh_spend_and_recovers_on_no_spend() {
        let db = db();
        db.write("t", |t| {
            let mut d = def("proxy", "litellm", None);
            d.cloud = None;
            d.selfhosted = Some(terrarium_core::selfhosted::SelfHosted {
                api: terrarium_core::selfhosted::Api::Anthropic,
                base_url: "https://proxy.example".into(),
                budget_usd: Some(100),
                ..Default::default()
            });
            subs::upsert_def(t, &d, SOURCE_API)?;
            cloud_spend::record(
                t,
                "a",
                "s",
                Some("proxy"),
                &cloud::month_key(Utc::now()),
                80.0,
            )?;
            let sub = subs::get(t, "proxy")?;
            let now = Utc::now();
            assert!(litellm_fallback(t, &sub, now)?);
            assert_eq!(write_window(t, &sub, now, "link")?.unwrap().used_pct, 80.0);
            for total in [90.0, 100.0, 100.0] {
                cloud_spend::record(t, "a", "s", Some("proxy"), &cloud::month_key(now), total)?;
                write_window(t, &sub, now, "link")?;
            }
            let mails: i64 = t.query_row(
                "SELECT count(*) FROM messages WHERE subject LIKE '%proxy%'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(mails, 3, "80, 90 and 100 notify once each");
            assert!(crate::store::usage::holds(t)?.is_empty());
            subs::insert_sample(t, "proxy", "key-spend", 10.0, None, &json!({}))?;
            assert!(!litellm_fallback(t, &sub, now)?);
            assert!(window(t, &sub, now)?.is_none());
            subs::set_probe_result(t, "proxy", Err("parse: no spend from LiteLLM"))?;
            let mut sub = subs::get(t, "proxy")?;
            assert!(litellm_fallback(t, &sub, now)?);
            assert_eq!(window(t, &sub, now)?.unwrap().used_usd, 100.0);
            sub.selfhosted.as_mut().unwrap().budget_usd = None;
            assert!(window(t, &sub, now)?.is_none());
            sub.probe_error = None;
            assert!(litellm_fallback(
                t,
                &sub,
                now + chrono::Duration::minutes(11)
            )?);
            Ok(())
        })
        .await
        .unwrap();
    }

    /// The window has used, remaining, and used_pct; the 80 percent line
    /// mails the operator once; 100 percent is used up; a new month starts
    /// at zero.
    #[tokio::test]
    async fn the_budget_window_and_its_alerts() {
        let db = db();
        let now = at("2026-11-10T12:00:00Z");
        db.write("t", move |t| {
            subs::upsert_def(t, &def("aws", "bedrock", Some(100)), SOURCE_API)?;
            subs::upsert_def(t, &def("max", "anthropic", None), SOURCE_API)?;
            cloud_spend::record(t, "a1", "s1", Some("aws"), "2026-11", 50.0)
        })
        .await
        .unwrap();
        async fn run(db: &Db, now: DateTime<Utc>) -> ApiResult<(Option<BudgetWindow>, i64)> {
            db.write("t", move |t| {
                let sub = subs::get(t, "aws")?;
                let w = write_window(t, &sub, now, "http://dash/subscriptions")?;
                let mails: i64 = t.query_row(
                    "SELECT count(*) FROM messages WHERE subject LIKE '%aws%'",
                    [],
                    |r| r.get(0),
                )?;
                Ok((w, mails))
            })
            .await
        }
        let (w, mails) = run(&db, now).await.unwrap();
        let w = w.unwrap();
        assert_eq!(
            (w.used_usd, w.remaining_usd, w.used_pct),
            (50.0, 50.0, 50.0)
        );
        assert!(!w.alert());
        assert_eq!(mails, 0);
        let s = db
            .read(|c| subs::latest_windows(c, "aws"))
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.window == "budget")
            .unwrap();
        assert_eq!(s.used_pct, 50.0);
        assert_eq!(s.resets_at.as_deref(), Some("2026-12-01T00:00:00Z"));
        assert_eq!(s.raw["remaining_usd"], 50.0);
        assert_eq!(s.raw["currency"], "USD");

        // 85 percent: the alert line mails once.
        db.write("t", |t| {
            cloud_spend::record(t, "a1", "s1", Some("aws"), "2026-11", 85.0)
        })
        .await
        .unwrap();
        let (w, mails) = run(&db, now).await.unwrap();
        assert!(w.unwrap().alert());
        assert_eq!(mails, 1);
        let (_, mails) = run(&db, now).await.unwrap();
        assert_eq!(mails, 1, "the line notifies once");

        // 100 percent: used up.
        db.write("t", |t| {
            cloud_spend::record(t, "a2", "s2", Some("aws"), "2026-11", 20.0)
        })
        .await
        .unwrap();
        let (w, _) = run(&db, now).await.unwrap();
        let w = w.unwrap();
        assert!(w.exhausted());
        assert_eq!((w.used_usd, w.remaining_usd), (105.0, 0.0));

        // A new month: the sessions count from their totals at its start.
        let (w, _) = run(&db, at("2026-12-01T00:00:01Z")).await.unwrap();
        assert_eq!(w.unwrap().used_pct, 0.0);

        // No budget, or not a cloud subscription: no window.
        let none = db
            .write("t", move |t| {
                let max = subs::get(t, "max")?;
                write_window(t, &max, now, "l")
            })
            .await
            .unwrap();
        assert!(none.is_none());
    }
}
