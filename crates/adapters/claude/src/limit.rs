//! The usage-limit signal of Claude Code (`docs/compat.md`, "Usage
//! limits").
//!
//! When the API refuses a turn, Claude Code ends the turn and fires the
//! `StopFailure` hook. Its payload (read in the 2.1.312 bundle, schema of
//! the hook input) is:
//!
//! ```json
//! {"hook_event_name": "StopFailure", "error": "rate_limit",
//!  "error_details": "<raw API message>",
//!  "last_assistant_message": "You've hit your limit · resets 5pm (UTC)"}
//! ```
//!
//! `error` is one of `authentication_failed`, `oauth_org_not_allowed`,
//! `account_on_hold`, `verification_required`, `billing_error`,
//! `rate_limit`, `overloaded`, `invalid_request`, `model_not_found`,
//! `server_error`, `unknown`, `max_output_tokens`, and
//! `cloud_credential_error`. A 429 of the plan gives `rate_limit`;
//! [`LIMIT_ERRORS`] are the values that stop the agent until a reset.
//!
//! `last_assistant_message` is the text that the TUI shows. For a plan
//! limit it ends with ` · resets <time> (<time zone>)`. The time is `5pm`
//! or `5:30pm` when the reset is within 24 hours, else `Oct 31, 5pm`, with
//! the year after the day when the year is not the current one
//! (`Oct 31, 2027, 5pm`). [`reset_time`] reads that form.

use std::str::FromStr;

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde_json::Value;
use terrarium_core::Event;

/// The `StopFailure` errors that are a usage limit or a rate limit.
pub const LIMIT_ERRORS: [&str; 2] = ["rate_limit", "billing_error"];

/// The longest message in the event, in characters.
const MESSAGE_MAX: usize = 300;

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// The text on one line, with no control characters, at most
/// [`MESSAGE_MAX`] characters.
pub fn one_line(text: &str) -> String {
    let flat: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let mut out: String = flat.chars().take(MESSAGE_MAX).collect();
    if flat.chars().count() > MESSAGE_MAX {
        out.push('…');
    }
    out
}

/// The [`Event::UsageLimit`] of a `StopFailure` payload, or `None` when
/// its `error` is not in [`LIMIT_ERRORS`].
pub fn usage_limit(body: &Value, now: DateTime<Utc>) -> Option<Event> {
    let error = s(body, "error").filter(|e| LIMIT_ERRORS.contains(e))?;
    let text = [s(body, "last_assistant_message"), s(body, "error_details")]
        .into_iter()
        .flatten()
        .map(one_line)
        .find(|t| !t.is_empty());
    let resets_at = text.as_deref().and_then(|t| reset_time(t, now));
    Some(Event::UsageLimit {
        message: text.unwrap_or_else(|| format!("the API answered `{error}`")),
        resets_at,
    })
}

/// `5pm`, `5:30pm`, `12am`.
fn clock(text: &str) -> Option<NaiveTime> {
    let t = text.trim().to_ascii_lowercase();
    let (num, pm) = if let Some(n) = t.strip_suffix("pm") {
        (n, true)
    } else {
        (t.strip_suffix("am")?, false)
    };
    let (h, m) = match num.trim().split_once(':') {
        Some((h, m)) => (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?),
        None => (num.trim().parse::<u32>().ok()?, 0),
    };
    if !(1..=12).contains(&h) {
        return None;
    }
    let h24 = match (h, pm) {
        (12, false) => 0,
        (12, true) => 12,
        (h, false) => h,
        (h, true) => h + 12,
    };
    NaiveTime::from_hms_opt(h24, m, 0)
}

/// `Oct 31` in `year`.
fn day(text: &str, year: i32) -> Option<NaiveDate> {
    let (mon, d) = text.trim().split_once(' ')?;
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let month = MONTHS
        .iter()
        .position(|m| mon.to_ascii_lowercase().starts_with(m))?;
    let month = u32::try_from(month).ok()? + 1;
    NaiveDate::from_ymd_opt(year, month, d.trim().parse().ok()?)
}

/// The reset time in a limit message: the last `resets <time> (<zone>)`.
/// `None` when the message has no such part, or when the zone is not an
/// IANA name.
pub fn reset_time(text: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let at = text.rfind("resets ")?;
    let rest = &text[at + "resets ".len()..];
    let open = rest.find(" (")?;
    let close = rest[open..].find(')')? + open;
    let zone = Tz::from_str(rest[open + 2..close].trim()).ok()?;
    let spec = rest[..open].trim();
    let local_now = now.with_timezone(&zone);
    let parts: Vec<&str> = spec.split(", ").collect();
    let (date, time) = match parts.as_slice() {
        [time] => (None, clock(time)?),
        [d, time] => (Some(day(d, local_now.year())?), clock(time)?),
        [d, y, time] => (Some(day(d, y.trim().parse().ok()?)?), clock(time)?),
        _ => return None,
    };
    let local = |d: NaiveDate| {
        zone.from_local_datetime(&d.and_time(time))
            .earliest()
            .map(|t| t.with_timezone(&Utc))
    };
    match date {
        Some(d) => local(d),
        None => {
            // Within 24 hours: today, or tomorrow when that time passed.
            let today = local(local_now.date_naive())?;
            if today > now {
                Some(today)
            } else {
                local(local_now.date_naive() + Duration::days(1))
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn reset_times_in_the_three_forms() {
        let now = at("2026-10-30T10:00:00Z");
        for (text, want) in [
            (
                "You've hit your limit · resets 5pm (UTC)",
                Some("2026-10-30T17:00:00Z"),
            ),
            // The time passed today: tomorrow.
            (
                "You've hit your limit · resets 9:30am (UTC)",
                Some("2026-10-31T09:30:00Z"),
            ),
            (
                "You've hit your limit · resets 12am (Europe/London)",
                Some("2026-10-31T00:00:00Z"),
            ),
            (
                "You've hit your weekly limit · resets Nov 3, 5pm (America/New_York)",
                Some("2026-11-03T22:00:00Z"),
            ),
            (
                "You've hit your limit · resets Jan 2, 2027, 8am (UTC)",
                Some("2027-01-02T08:00:00Z"),
            ),
            ("You've hit your limit", None),
            ("You've hit your limit · resets 5pm (Mars/Base)", None),
            ("You've hit your limit · resets soon (UTC)", None),
        ] {
            assert_eq!(reset_time(text, now), want.map(at), "{text}");
        }
    }

    #[test]
    fn only_limit_errors_give_the_event() {
        let now = at("2026-10-30T10:00:00Z");
        let ev = usage_limit(
            &json!({"error": "rate_limit",
                    "last_assistant_message": "You've hit your limit · resets 5pm (UTC)\n"}),
            now,
        );
        assert_eq!(
            ev,
            Some(Event::UsageLimit {
                message: "You've hit your limit · resets 5pm (UTC)".into(),
                resets_at: Some(at("2026-10-30T17:00:00Z")),
            })
        );
        let ev = usage_limit(&json!({"error": "billing_error"}), now);
        assert_eq!(
            ev,
            Some(Event::UsageLimit {
                message: "the API answered `billing_error`".into(),
                resets_at: None,
            })
        );
        for error in ["overloaded", "server_error", "unknown"] {
            assert_eq!(usage_limit(&json!({"error": error}), now), None, "{error}");
        }
        let long = "x".repeat(400);
        let Some(Event::UsageLimit { message, .. }) =
            usage_limit(&json!({"error": "rate_limit", "error_details": long}), now)
        else {
            panic!("no event");
        };
        assert_eq!(message.chars().count(), MESSAGE_MAX + 1);
    }
}
