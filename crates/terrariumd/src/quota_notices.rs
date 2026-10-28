//! The quota notices to the operator (PLAN §16.2, docs/api.md §16.6).
//!
//! When a window of a subscription goes up past a line, the operator gets
//! one mail from `system`. The mail is an open item of the Inbox (kind
//! `mail`) until the operator acknowledges it. No banner shows. There are
//! three lines: the `alert_at` percent of the subscription (default 80),
//! 90, and 100 (used up). An `alert_at` of 90 or more gives no 90 line; an
//! `alert_at` of 0 (the API refuses it, a file can have it) gives no
//! `alert_at` line.
//!
//! Each line notifies once in a window period. The table `quota_notices`
//! keeps one row for each `(subscription, window, line)` that notified, so
//! repeated samples and a daemon restart send nothing new. The row goes,
//! and the line can notify again, when:
//!
//! - the window drops [`REARM_GAP`] points or more under the line (the
//!   window reset), or
//! - the `resets_at` of the row has passed, and the sample is from a new
//!   period: it has no `resets_at`, or one more than [`PERIOD_SLACK`]
//!   after the stored one. A provider that moves `resets_at` by a second
//!   between probes does not start a new period.
//!
//! One sample that passes two lines sends one mail, for the higher line.
//! The 100 line mail says that the window is used up and names the agents
//! that [`crate::quota_policy`] holds; the policy calls [`check`] after the
//! holds. Each mail also emits one `quota.alert` event with the
//! `threshold`, and `level: alert` (under 100) or `level: exhausted`.
//!
//! The priority is normal at all lines: mail has no priority, and the
//! operator wants no popup. A used-up window is not an outage: the held
//! turns go out after the reset, and the auto-swap moves pooled agents.

use chrono::{DateTime, Duration, Utc};
use rusqlite::{OptionalExtension, params};
use serde_json::json;

use crate::db::{Tx, now as now_str};
use crate::error::ApiResult;
use crate::events::EventDraft;
use crate::store::agents;
use crate::store::mail::{self, Address, Message, Outgoing};
use crate::store::subscriptions::Subscription;
use crate::store::usage::{self as ustore, UsageFilter};

/// The second line, in percent.
pub const SECOND_LINE: u8 = 90;
/// The used-up line, in percent.
pub const USED_UP: u8 = 100;
/// A window this many points under a line is a new period for the line.
pub const REARM_GAP: f64 = 10.0;
/// Two reset times this close are the same period.
pub const PERIOD_SLACK: Duration = Duration::minutes(5);
/// The most agents that one mail names.
const MAX_AGENTS: usize = 5;

/// The lines of a subscription, lowest first.
pub fn lines(alert_at: u8) -> Vec<u8> {
    let mut v = Vec::new();
    if (1..USED_UP).contains(&alert_at) {
        v.push(alert_at);
    }
    if alert_at < SECOND_LINE {
        v.push(SECOND_LINE);
    }
    v.push(USED_UP);
    v
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Is a sample with reset `sample` in a new period after the stored
/// reset `stored`?
fn new_period(stored: Option<&str>, sample: Option<&str>, now: DateTime<Utc>) -> bool {
    let Some(stored) = stored.and_then(parse_time) else {
        return false;
    };
    if stored > now {
        return false;
    }
    match sample.and_then(parse_time) {
        None => true,
        Some(s) => s > stored + PERIOD_SLACK,
    }
}

/// Update the rows of one window for a sample at `pct` percent. Returns
/// the highest line that the sample passed and that did not notify in
/// this period. The caller sends the mail.
pub fn crossing(
    t: &Tx<'_>,
    subscription: &str,
    alert_at: u8,
    window: &str,
    pct: f64,
    resets_at: Option<&str>,
    now: DateTime<Utc>,
) -> ApiResult<Option<u8>> {
    let mut passed = None;
    for line in lines(alert_at) {
        let level = f64::from(line);
        let stored: Option<Option<String>> = t
            .query_row(
                "SELECT resets_at FROM quota_notices
                 WHERE subscription = ?1 AND window = ?2 AND threshold = ?3",
                params![subscription, window, line],
                |r| r.get(0),
            )
            .optional()?;
        let mut notified = stored.is_some();
        if let Some(stored) = &stored
            && (pct <= level - REARM_GAP || new_period(stored.as_deref(), resets_at, now))
        {
            t.execute(
                "DELETE FROM quota_notices
                 WHERE subscription = ?1 AND window = ?2 AND threshold = ?3",
                params![subscription, window, line],
            )?;
            notified = false;
        }
        if pct >= level && !notified {
            t.execute(
                "INSERT INTO quota_notices (subscription, window, threshold, resets_at,
                                            used_pct, at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![subscription, window, line, resets_at, pct, now_str()],
            )?;
            passed = Some(line);
        }
    }
    Ok(passed)
}

/// `2h 14m`, `3d 4h`, `12m`, or `under a minute`.
fn span(d: Duration) -> String {
    let m = d.num_minutes();
    match m {
        ..=0 => "under a minute".into(),
        1..=59 => format!("{m}m"),
        60..=1439 => format!("{}h {}m", m / 60, m % 60),
        _ => format!("{}d {}h", m / 1440, (m % 1440) / 60),
    }
}

/// `1.2M`, `340k`, or `900`.
#[allow(clippy::cast_precision_loss)]
fn tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{}k", n / 1_000),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// The length of a window by its name: `5h`, `7d`, `30m`. `None` for a
/// window with no length in its name (`credits`, `balance`).
fn window_length(window: &str) -> Option<Duration> {
    let (n, unit) = window.split_at(window.find(|c: char| !c.is_ascii_digit())?);
    let n: i64 = n.parse().ok()?;
    match unit {
        "m" => Some(Duration::minutes(n)),
        "h" => Some(Duration::hours(n)),
        "d" => Some(Duration::days(n)),
        "w" => Some(Duration::weeks(n)),
        _ => None,
    }
}

/// The line about the agents: the biggest spenders on the subscription
/// in the window, else the agents on it.
fn agents_line(t: &Tx<'_>, sub: &str, window: &str, now: DateTime<Utc>) -> ApiResult<String> {
    let (since, label) = match window_length(window) {
        Some(len) => (
            (now - len).to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            format!("in the last {window}"),
        ),
        None => (ustore::today_start(), "today (UTC)".to_owned()),
    };
    let spent = ustore::by_agent(
        t,
        &UsageFilter {
            subscription: Some(sub.to_owned()),
            since: Some(since),
            ..UsageFilter::default()
        },
    )?;
    let spent: Vec<_> = spent.into_iter().filter(|(_, s)| s.tokens() > 0).collect();
    let more = |n: usize| {
        if n > MAX_AGENTS {
            format!(", and {} more", n - MAX_AGENTS)
        } else {
            String::new()
        }
    };
    if !spent.is_empty() {
        let names: Vec<String> = spent
            .iter()
            .take(MAX_AGENTS)
            .map(|(id, s)| {
                let name = agents::known_name(t, id)?.unwrap_or_else(|| id.clone());
                Ok(format!("{name} ({} tokens)", tokens(s.tokens())))
            })
            .collect::<ApiResult<_>>()?;
        return Ok(format!(
            "The agents that spent the most {label}: {}{}.",
            names.join(", "),
            more(spent.len())
        ));
    }
    let on = ustore::agents_on(t, sub)?;
    if on.is_empty() {
        return Ok("No agent is on this subscription now.".into());
    }
    let names: Vec<String> = on
        .iter()
        .take(MAX_AGENTS)
        .map(|(_, name, project)| format!("{project}/{name}"))
        .collect();
    Ok(format!(
        "The agents on this subscription: {}{}.",
        names.join(", "),
        more(on.len())
    ))
}

/// The line about the held agents of a used-up subscription.
fn held_line(t: &Tx<'_>, sub: &str) -> ApiResult<String> {
    let cause = format!("quota:{sub}");
    let held: Vec<String> = ustore::holds(t)?
        .into_iter()
        .filter(|h| h.cause == cause)
        .map(|h| Ok(agents::known_name(t, &h.agent)?.unwrap_or(h.agent)))
        .collect::<ApiResult<_>>()?;
    if held.is_empty() {
        return Ok("No agent is held: no agent on it waits for the reset.".into());
    }
    let more = if held.len() > MAX_AGENTS {
        format!(", and {} more", held.len() - MAX_AGENTS)
    } else {
        String::new()
    };
    let names: Vec<&str> = held.iter().take(MAX_AGENTS).map(String::as_str).collect();
    Ok(format!(
        "New turns of these agents wait for the reset: {}{more}.",
        names.join(", ")
    ))
}

/// Check one window of `sub` at `pct` percent (a reset that has passed
/// counts as 0). When it passed a line that did not notify in this
/// period, mail the operator and emit a `quota.alert` event. `link` is
/// the dashboard page of the subscriptions. Returns the mail.
pub fn check(
    t: &mut Tx<'_>,
    sub: &Subscription,
    window: &str,
    pct: f64,
    resets_at: Option<&str>,
    link: &str,
    now: DateTime<Utc>,
) -> ApiResult<Option<Message>> {
    let Some(line) = crossing(t, &sub.name, sub.alert_at, window, pct, resets_at, now)? else {
        return Ok(None);
    };
    let reset = match resets_at.and_then(parse_time) {
        Some(r) => format!(
            "The window resets in {} ({}).",
            span(r - now),
            r.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        ),
        None => "The provider gives no reset time for this window.".into(),
    };
    let used_up = line >= USED_UP;
    let (subject, body) = if used_up {
        (
            format!("Quota: {} {window} window is used up", sub.name),
            format!(
                "Subscription `{}` ({}) has used all of its {window} window.\n{reset}\n{}\n\n\
                 Move agents to another account with `terrarium admin agent swap <name> \
                 <subscription>`, or wait for the reset.\n\nOpen: {link}\n",
                sub.name,
                sub.provider,
                held_line(t, &sub.name)?
            ),
        )
    } else {
        let next = lines(sub.alert_at)
            .into_iter()
            .find(|l| *l > line && *l < USED_UP)
            .map(|l| format!("You get one more message at {l}%.\n"))
            .unwrap_or_default();
        (
            format!("Quota: {} {window} window at {pct:.0}%", sub.name),
            format!(
                "Subscription `{}` ({}) has used {pct:.0}% of its {window} window. It passed \
                 the {line}% line.\n{reset}\n{}\n{next}\nMove agents to another account with \
                 `terrarium admin agent swap <name> <subscription>`, give them an account pool \
                 for the auto-swap, or wait for the reset.\n\nOpen: {link}\n",
                sub.name,
                sub.provider,
                agents_line(t, &sub.name, window, now)?
            ),
        )
    };
    let m = mail::send(
        t,
        Outgoing {
            from: &Address::System,
            to: &Address::Operator,
            subject: &subject,
            body: &body,
            thread: None,
            in_reply_to: None,
        },
    )?;
    t.execute(
        "UPDATE quota_notices SET message = ?4
         WHERE subscription = ?1 AND window = ?2 AND threshold = ?3",
        params![sub.name, window, line, m.id],
    )?;
    t.emit(EventDraft::new(
        "quota.alert",
        subject,
        json!({"subscription": sub.name, "window": window, "used_pct": pct,
               "resets_at": resets_at,
               "level": if used_up { "exhausted" } else { "alert" },
               "alert_at": sub.alert_at, "threshold": line, "message": m.id}),
    ))?;
    Ok(Some(m))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::events::EventHub;
    use crate::store::subscriptions as subs;
    use terrarium_core::config::SubscriptionDef;

    fn open(path: &std::path::Path) -> Db {
        let mut conn = crate::db::open(path).unwrap();
        crate::db::migrate(&mut conn).unwrap();
        Db::new(conn, EventHub::default())
    }

    async fn add_sub(db: &Db, alert_at: u8) {
        db.write("operator", move |t| {
            subs::upsert_def(
                t,
                &SubscriptionDef {
                    name: "max".into(),
                    provider: "anthropic".into(),
                    plan: None,
                    harnesses: Vec::new(),
                    probe: None,
                    alert_at,
                },
                "api",
            )
        })
        .await
        .unwrap();
    }

    /// Feed one sample; return the subject of the new mail, if any.
    async fn feed(db: &Db, pct: f64, reset: &str, now: DateTime<Utc>) -> Option<String> {
        let reset = reset.to_owned();
        db.write("system", move |t| {
            let sub = subs::get(t, "max")?;
            Ok(check(
                t,
                &sub,
                "5h",
                pct,
                Some(&reset),
                "http://x/subscriptions",
                now,
            )?
            .map(|m| m.subject))
        })
        .await
        .unwrap()
    }

    async fn operator_mail(db: &Db) -> usize {
        db.read(|c| Ok(mail::operator_mail(c, true, 100)?.len()))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn one_mail_at_each_line_across_restarts_and_resets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let db = open(&path);
        add_sub(&db, 80).await;
        let now = Utc::now();
        let r1 = (now + Duration::hours(2)).to_rfc3339();
        assert_eq!(feed(&db, 79.0, &r1, now).await, None);
        let first = feed(&db, 81.0, &r1, now).await.unwrap();
        assert_eq!(first, "Quota: max 5h window at 81%");
        assert_eq!(feed(&db, 85.0, &r1, now).await, None);
        // A reset time that moves by a second is the same period.
        let r1b = (now + Duration::hours(2) + Duration::seconds(1)).to_rfc3339();
        assert_eq!(feed(&db, 86.0, &r1b, now).await, None);
        assert_eq!(
            feed(&db, 91.0, &r1, now).await.as_deref(),
            Some("Quota: max 5h window at 91%")
        );
        assert_eq!(feed(&db, 95.0, &r1, now).await, None);
        assert_eq!(operator_mail(&db).await, 2);

        // A restart: the rows stay.
        drop(db);
        let db = open(&path);
        assert_eq!(feed(&db, 95.0, &r1, now).await, None);

        // The window resets: the next period notifies again.
        let later = now + Duration::hours(3);
        let r2 = (later + Duration::hours(5)).to_rfc3339();
        assert_eq!(
            feed(&db, 82.0, &r2, later).await.as_deref(),
            Some("Quota: max 5h window at 82%")
        );
        // A drop under the line (by the gap) re-arms it too.
        assert_eq!(feed(&db, 5.0, &r2, later).await, None);
        assert!(feed(&db, 80.0, &r2, later).await.is_some());
        assert_eq!(operator_mail(&db).await, 4);
        // A small dip does not re-arm.
        assert_eq!(feed(&db, 78.0, &r2, later).await, None);
        assert_eq!(feed(&db, 81.0, &r2, later).await, None);

        let alerts: i64 = db
            .read(|c| {
                Ok(c.query_row(
                    "SELECT count(*) FROM events WHERE kind = 'quota.alert'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(alerts, 4);
    }

    #[tokio::test]
    async fn the_mail_says_the_window_the_reset_and_the_next_line() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir.path().join("t.db"));
        add_sub(&db, 80).await;
        let now = Utc::now();
        let r =
            (now + Duration::hours(2) + Duration::minutes(14) + Duration::seconds(30)).to_rfc3339();
        feed(&db, 81.0, &r, now).await.unwrap();
        let m = db
            .read(|c| Ok(mail::operator_mail(c, true, 10)?.remove(0)))
            .await
            .unwrap();
        assert_eq!(m.from_kind, "system");
        assert!(m.body.contains("used 81% of its 5h window"), "{}", m.body);
        assert!(m.body.contains("passed the 80% line"), "{}", m.body);
        assert!(m.body.contains("resets in 2h 14m"), "{}", m.body);
        assert!(m.body.contains("one more message at 90%"), "{}", m.body);
        assert!(
            m.body.contains("No agent is on this subscription"),
            "{}",
            m.body
        );
        assert!(
            m.body.ends_with("Open: http://x/subscriptions\n"),
            "{}",
            m.body
        );
    }

    #[tokio::test]
    async fn one_line_when_alert_at_is_90_or_more() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir.path().join("t.db"));
        add_sub(&db, 90).await;
        let now = Utc::now();
        let r = (now + Duration::hours(2)).to_rfc3339();
        assert_eq!(feed(&db, 85.0, &r, now).await, None);
        assert!(feed(&db, 91.0, &r, now).await.is_some());
        assert_eq!(feed(&db, 99.0, &r, now).await, None);
        assert_eq!(operator_mail(&db).await, 1);
        // The used-up line is apart.
        assert!(feed(&db, 100.0, &r, now).await.is_some());
        assert_eq!(operator_mail(&db).await, 2);
    }

    #[tokio::test]
    async fn a_jump_past_two_lines_sends_one_mail() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir.path().join("t.db"));
        add_sub(&db, 80).await;
        let now = Utc::now();
        let r = (now + Duration::hours(2)).to_rfc3339();
        assert!(feed(&db, 93.0, &r, now).await.is_some());
        assert_eq!(feed(&db, 94.0, &r, now).await, None);
        assert_eq!(operator_mail(&db).await, 1);
    }

    #[tokio::test]
    async fn a_used_up_window_sends_one_mail_with_the_held_agents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let db = open(&path);
        add_sub(&db, 80).await;
        db.write("system", |t| {
            ustore::add_hold(t, "01HELDAGENT", "quota:max", None, "used up", false)?;
            ustore::add_hold(t, "01OTHER", "quota:other", None, "used up", false)?;
            Ok(())
        })
        .await
        .unwrap();
        let now = Utc::now();
        let r = (now + Duration::hours(2)).to_rfc3339();
        assert_eq!(
            feed(&db, 100.0, &r, now).await.as_deref(),
            Some("Quota: max 5h window is used up")
        );
        assert_eq!(feed(&db, 100.0, &r, now).await, None);
        drop(db);
        let db = open(&path);
        assert_eq!(feed(&db, 100.0, &r, now).await, None);
        let m = db.read(|c| mail::operator_mail(c, true, 10)).await.unwrap();
        assert_eq!(m.len(), 1);
        assert!(
            m[0].body.contains("has used all of its 5h window"),
            "{}",
            m[0].body
        );
        assert!(
            m[0].body.contains("wait for the reset: 01HELDAGENT."),
            "{}",
            m[0].body
        );
        assert!(!m[0].body.contains("01OTHER"), "{}", m[0].body);
        let level: String = db
            .read(|c| {
                Ok(c.query_row(
                    "SELECT json_extract(payload, '$.level') FROM events
                     WHERE kind = 'quota.alert'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(level, "exhausted");
    }

    #[test]
    fn lines_and_helpers() {
        assert_eq!(lines(80), [80, 90, 100]);
        assert_eq!(lines(90), [90, 100]);
        assert_eq!(lines(95), [95, 100]);
        assert_eq!(lines(100), [100]);
        assert_eq!(lines(0), [90, 100]);
        assert_eq!(span(Duration::minutes(134)), "2h 14m");
        assert_eq!(span(Duration::hours(76)), "3d 4h");
        assert_eq!(tokens(1_234_567), "1.2M");
        assert_eq!(tokens(34_500), "34k");
        assert_eq!(window_length("5h"), Some(Duration::hours(5)));
        assert_eq!(window_length("7d"), Some(Duration::days(7)));
        assert_eq!(window_length("credits"), None);
    }
}
