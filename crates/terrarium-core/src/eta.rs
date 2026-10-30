//! The ETA of a card (PLAN §13, docs/api.md §8): when the assignee expects
//! the work to be done.
//!
//! The daemon and the CLI read the same forms: an RFC 3339 time, or a
//! duration from now with one or more parts (`45m`, `2h30m`, `1d`). The
//! time must be in the future and not more than [`MAX_DAYS`] days away.

use chrono::{DateTime, Duration, Utc};

/// The largest ETA, in days from now.
pub const MAX_DAYS: i64 = 30;

/// The accepted forms, for error messages.
pub const FORMS: &str = "a duration from now such as 45m, 2h30m, or 1d, or an RFC 3339 time \
                         such as 2026-10-25T19:53:00+10:00";

/// Parse a duration of one or more `<number><unit>` parts, with the units
/// `d`, `h`, `m`, and `s` (`2h30m`, `1d4h`, `90s`). A bare number has no
/// unit and is an error. The sum must be more than zero.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let bad = || format!("`{s}` is not a duration such as 45m, 2h30m, or 1d");
    if s.is_empty() {
        return Err(bad());
    }
    let mut secs: i64 = 0;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let mult = match c.to_ascii_lowercase() {
            'd' => 86_400,
            'h' => 3600,
            'm' => 60,
            's' => 1,
            _ => return Err(bad()),
        };
        let n: i64 = num.parse().map_err(|_| bad())?;
        num.clear();
        secs = n
            .checked_mul(mult)
            .and_then(|p| secs.checked_add(p))
            .ok_or_else(bad)?;
    }
    if !num.is_empty() {
        return Err(bad());
    }
    if secs == 0 {
        return Err(format!("`{s}`: the duration must be more than zero"));
    }
    Ok(Duration::seconds(secs))
}

/// Resolve an ETA value at `now`: an RFC 3339 time, or a duration from
/// `now`. The time must be after `now` and not more than [`MAX_DAYS`]
/// days after it.
pub fn resolve(v: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
    let v = v.trim();
    let at = match DateTime::parse_from_rfc3339(v) {
        Ok(t) => t.with_timezone(&Utc),
        Err(_) => {
            let d = parse_duration(v).map_err(|e| format!("bad ETA: {e}; use {FORMS}"))?;
            if d > Duration::days(MAX_DAYS) {
                return Err(too_far(v));
            }
            now + d
        }
    };
    if at <= now {
        return Err(format!(
            "the ETA `{v}` is in the past; give a time in the future"
        ));
    }
    if at - now > Duration::days(MAX_DAYS) {
        return Err(too_far(v));
    }
    Ok(at)
}

fn too_far(v: &str) -> String {
    format!("the ETA `{v}` is more than {MAX_DAYS} days away; split the work into smaller cards")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn durations_have_one_or_more_parts() {
        assert_eq!(parse_duration("45m").unwrap(), Duration::minutes(45));
        assert_eq!(parse_duration("2h30m").unwrap(), Duration::minutes(150));
        assert_eq!(parse_duration("1d").unwrap(), Duration::days(1));
        assert_eq!(parse_duration("1d4h").unwrap(), Duration::hours(28));
        assert_eq!(parse_duration(" 90s ").unwrap(), Duration::seconds(90));
        assert_eq!(parse_duration("1H").unwrap(), Duration::hours(1));
        for bad in [
            "", "45", "2h30", "h", "1w", "0m", "0h0m", "1.5h", "-1h", "1 h",
        ] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn an_eta_is_in_the_future_and_at_most_30_days_away() {
        let now = DateTime::parse_from_rfc3339("2026-10-30T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(resolve("2h30m", now).unwrap(), now + Duration::minutes(150));
        assert_eq!(
            resolve("2026-10-30T21:00:00+10:00", now).unwrap(),
            now + Duration::hours(1)
        );
        assert!(resolve("30d", now).is_ok());
        let e = resolve("31d", now).unwrap_err();
        assert!(e.contains("30 days"), "{e}");
        let e = resolve("2026-12-30T10:00:00Z", now).unwrap_err();
        assert!(e.contains("30 days"), "{e}");
        let e = resolve("2026-10-30T09:59:00Z", now).unwrap_err();
        assert!(e.contains("past"), "{e}");
        let e = resolve("soon", now).unwrap_err();
        assert!(e.contains("RFC 3339"), "{e}");
    }
}
