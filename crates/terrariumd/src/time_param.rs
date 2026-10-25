//! The time query values `?since=` and `?until=` (api.md §2.1).
//!
//! Stores keep `at` as RFC 3339 UTC with milliseconds (`db::now`) and
//! compare it as TEXT. A value with another offset, or another precision,
//! compares wrongly as text, so every endpoint normalises the value here
//! first.

use chrono::{DateTime, Utc};

use crate::error::{ApiError, ApiResult};

/// The accepted forms, for error messages.
pub const FORMS: &str = "an RFC 3339 time with an offset (2026-10-25T19:53:00+10:00 or \
                         2026-10-25T09:53:00Z), or a duration back from now (90s, 30m, 2h, 1d)";

/// The stored form of a time: UTC, milliseconds, `Z`.
pub fn stored(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Parse a time value: RFC 3339 with any offset, or a duration with a unit
/// (`30m`) that counts back from now. `None` for anything else, a bare
/// number and a bare clock time included.
pub fn parse_time(v: &str) -> Option<DateTime<Utc>> {
    let v = v.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(v) {
        return Some(t.with_timezone(&Utc));
    }
    // A bare number is a `seq` on the event endpoints; a duration needs a unit.
    if !v.ends_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    let d = crate::settings::parse_duration(v).ok()?;
    Utc::now().checked_sub_signed(chrono::Duration::from_std(d).ok()?)
}

/// Parse the query value `name` into the stored form. A bad value is
/// `400 bad_request` with the accepted forms.
pub fn parse_time_param(name: &str, v: &str) -> ApiResult<String> {
    parse_time(v)
        .map(stored)
        .ok_or_else(|| ApiError::bad_request(format!("bad `{name}` `{v}`: use {FORMS}")))
}

/// [`parse_time_param`] for an optional value. Absent or empty is `None`.
pub fn opt_time_param(name: &str, v: Option<&str>) -> ApiResult<Option<String>> {
    v.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| parse_time_param(name, s))
        .transpose()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn offsets_normalise_to_utc_millis() {
        assert_eq!(
            parse_time_param("since", "2026-10-25T19:53:00+10:00").unwrap(),
            "2026-10-25T09:53:00.000Z"
        );
        assert_eq!(
            parse_time_param("since", "2026-10-25T09:53:00Z").unwrap(),
            "2026-10-25T09:53:00.000Z"
        );
        assert_eq!(
            parse_time_param("until", "2026-10-25T09:53:00.123456-02:30").unwrap(),
            "2026-10-25T12:23:00.123Z"
        );
    }

    #[test]
    fn durations_count_back_from_now() {
        let before = Utc::now();
        let t = parse_time("2h").unwrap();
        let d = before - t;
        assert!(d >= chrono::Duration::minutes(119) && d <= chrono::Duration::minutes(121));
        assert!(parse_time("1d").is_some());
        assert!(parse_time("30m").is_some());
    }

    #[test]
    fn clock_times_numbers_and_words_are_refused() {
        for bad in [
            "19:53",
            "19:53:00",
            "3600",
            "0m",
            "soon",
            "latest",
            "2026-10-25",
        ] {
            let e = parse_time_param("since", bad).unwrap_err();
            assert!(e.to_string().contains("RFC 3339"), "{bad}: {e}");
        }
        assert_eq!(opt_time_param("since", Some(" ")).unwrap(), None);
        assert_eq!(opt_time_param("since", None).unwrap(), None);
    }
}
