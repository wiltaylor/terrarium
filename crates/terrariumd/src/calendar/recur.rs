//! The recurrence rules of calendar events (docs/api.md §33.2): a small
//! subset of the iCalendar RRULE (RFC 5545), and its expansion into
//! occurrences.
//!
//! A rule has a frequency (`daily`, `weekly`, `monthly`), an interval, the
//! weekdays of a weekly rule, the month day of a monthly rule, an end
//! (`until` or `count`), and a time zone. The rule repeats the wall clock
//! time of the first start in its zone, so a 09:00 event stays at 09:00
//! local time across a DST change. A local time that the zone skips (the
//! spring gap) moves one hour on; a local time that comes twice (the
//! autumn overlap) takes the first.
//!
//! The rule takes the RRULE text too: `FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE;
//! COUNT=6`. [`Rule::parse_rrule`] refuses each part that this subset does
//! not have, with a message that names it.
//!
//! Rules of the expansion, as in RFC 5545:
//!
//! - A weekly rule repeats in weeks that start on Monday (`WKST=MO`).
//! - A monthly rule skips a month that has no such day (31 in April). A
//!   month day of `-1` is the last day of each month.
//! - The first start need not match the rule: the first occurrence is the
//!   first date on or after it that matches.
//! - `count` counts the occurrences that the rule makes, the skipped ones
//!   (exceptions) too.
//! - `until` as a date ends the rule after that local date; as a time, no
//!   occurrence starts after it.

use std::collections::BTreeSet;

use chrono::{
    DateTime, Datelike, Duration, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc,
    Weekday,
};
use chrono_tz::Tz;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The highest interval.
pub const MAX_INTERVAL: u32 = 999;
/// The highest count.
pub const MAX_COUNT: u32 = 10_000;
/// The most candidate dates that one expansion looks at.
const MAX_STEPS: usize = 200_000;

/// The frequency of a rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Freq {
    Daily,
    Weekly,
    Monthly,
}

impl Freq {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
        }
    }
}

/// A weekday as two letters: `mo`, `tu`, `we`, `th`, `fr`, `sa`, `su`.
/// A full name (`monday`) and three letters (`mon`) read too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Day(pub u8);

impl Day {
    pub fn weekday(self) -> Weekday {
        match self.0 {
            0 => Weekday::Mon,
            1 => Weekday::Tue,
            2 => Weekday::Wed,
            3 => Weekday::Thu,
            4 => Weekday::Fri,
            5 => Weekday::Sat,
            _ => Weekday::Sun,
        }
    }

    pub fn of(w: Weekday) -> Self {
        Self(w.num_days_from_monday() as u8)
    }

    pub fn code(self) -> &'static str {
        ["mo", "tu", "we", "th", "fr", "sa", "su"][usize::from(self.0.min(6))]
    }

    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        let names = [
            "monday",
            "tuesday",
            "wednesday",
            "thursday",
            "friday",
            "saturday",
            "sunday",
        ];
        names.iter().enumerate().find_map(|(i, n)| {
            (s.len() >= 2 && n.starts_with(&s) && (s.len() <= 3 || s == *n)).then_some(Day(i as u8))
        })
    }
}

impl Serialize for Day {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.code())
    }
}

impl<'de> Deserialize<'de> for Day {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Day::parse(&s).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "bad weekday `{s}`: use mo, tu, we, th, fr, sa, or su"
            ))
        })
    }
}

/// A recurrence rule (the stored form, and the JSON of the API).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub freq: Freq,
    #[serde(default = "one")]
    pub interval: u32,
    /// The days of a weekly rule. Empty: the weekday of the first start.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub weekdays: Vec<Day>,
    /// The day of a monthly rule: 1 to 31, or -1 for the last day.
    /// Absent: the day of the first start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub month_day: Option<i32>,
    /// The end: a local date (`2026-12-31`, that day included) or an RFC
    /// 3339 time (no occurrence starts after it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    /// The end: the number of occurrences.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    /// The IANA zone whose wall clock the rule repeats. Absent: the zone
    /// of the event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tz: Option<String>,
}

fn one() -> u32 {
    1
}

/// The end of a rule after [`Rule::check`].
#[derive(Debug, Clone, Copy, PartialEq)]
enum Until {
    Date(NaiveDate),
    Instant(DateTime<Utc>),
}

/// Parse an IANA zone name (`UTC`, `Europe/London`).
pub fn zone(name: &str) -> Result<Tz, String> {
    name.trim().parse::<Tz>().map_err(|_| {
        format!("unknown time zone `{name}`: use an IANA name such as UTC or Europe/London")
    })
}

fn parse_until(s: &str) -> Result<Until, String> {
    let s = s.trim();
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(Until::Date(d));
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(Until::Instant(t.with_timezone(&Utc)));
    }
    Err(format!(
        "bad until `{s}`: use a date (2026-12-31) or an RFC 3339 time"
    ))
}

impl Rule {
    /// Check the rule. The error names the field (`recurrence.<field>:
    /// ...`); the caller makes it a `422 invalid`.
    pub fn check(&self) -> Result<(), String> {
        if !(1..=MAX_INTERVAL).contains(&self.interval) {
            return Err(format!("interval: use 1 to {MAX_INTERVAL}"));
        }
        if !self.weekdays.is_empty() && self.freq != Freq::Weekly {
            return Err("weekdays: only a weekly rule has weekdays".into());
        }
        if let Some(d) = self.month_day {
            if self.freq != Freq::Monthly {
                return Err("month_day: only a monthly rule has a month day".into());
            }
            if !(d == -1 || (1..=31).contains(&d)) {
                return Err("month_day: use 1 to 31, or -1 for the last day".into());
            }
        }
        if self.until.is_some() && self.count.is_some() {
            return Err("until: give until or count, not both".into());
        }
        if let Some(c) = self.count
            && !(1..=MAX_COUNT).contains(&c)
        {
            return Err(format!("count: use 1 to {MAX_COUNT}"));
        }
        if let Some(u) = &self.until {
            parse_until(u).map_err(|e| format!("until: {e}"))?;
        }
        if let Some(z) = &self.tz {
            zone(z).map_err(|e| format!("tz: {e}"))?;
        }
        Ok(())
    }

    /// The rule with sorted, unique weekdays.
    pub fn normalized(mut self) -> Self {
        let days: BTreeSet<Day> = self.weekdays.iter().copied().collect();
        self.weekdays = days.into_iter().collect();
        self
    }

    /// Read an RRULE text (`FREQ=WEEKLY;BYDAY=MO,FR;COUNT=4`, with or
    /// without `RRULE:`). A part that this subset does not have is an
    /// error that names it.
    pub fn parse_rrule(text: &str) -> Result<Self, String> {
        let t = text.trim();
        let t = t
            .strip_prefix("RRULE:")
            .or_else(|| t.strip_prefix("rrule:"))
            .unwrap_or(t);
        let mut freq = None;
        let mut rule = Rule {
            freq: Freq::Daily,
            interval: 1,
            weekdays: Vec::new(),
            month_day: None,
            until: None,
            count: None,
            tz: None,
        };
        for part in t.split(';').map(str::trim).filter(|p| !p.is_empty()) {
            let Some((k, v)) = part.split_once('=') else {
                return Err(format!("bad RRULE part `{part}`: use NAME=VALUE"));
            };
            let k = k.trim().to_ascii_uppercase();
            let v = v.trim();
            match k.as_str() {
                "FREQ" => {
                    freq = Some(match v.to_ascii_uppercase().as_str() {
                        "DAILY" => Freq::Daily,
                        "WEEKLY" => Freq::Weekly,
                        "MONTHLY" => Freq::Monthly,
                        other => {
                            return Err(format!(
                                "FREQ={other} is not supported: use DAILY, WEEKLY, or MONTHLY"
                            ));
                        }
                    })
                }
                "INTERVAL" => {
                    rule.interval = v
                        .parse()
                        .map_err(|_| format!("bad INTERVAL `{v}`: use a whole number"))?
                }
                "COUNT" => {
                    rule.count = Some(
                        v.parse()
                            .map_err(|_| format!("bad COUNT `{v}`: use a whole number"))?,
                    )
                }
                "UNTIL" => rule.until = Some(rrule_until(v)?),
                "BYDAY" => {
                    for d in v.split(',') {
                        let d = d.trim();
                        if d.len() != 2 {
                            return Err(format!(
                                "BYDAY `{d}` is not supported: use two-letter days (MO,WE) \
                                 with no number in front"
                            ));
                        }
                        rule.weekdays
                            .push(Day::parse(d).ok_or_else(|| format!("bad BYDAY day `{d}`"))?);
                    }
                }
                "BYMONTHDAY" => {
                    if v.contains(',') {
                        return Err("BYMONTHDAY with more than one day is not supported".into());
                    }
                    rule.month_day = Some(
                        v.parse()
                            .map_err(|_| format!("bad BYMONTHDAY `{v}`: use 1 to 31 or -1"))?,
                    )
                }
                "WKST" => {
                    if !v.eq_ignore_ascii_case("MO") {
                        return Err(format!("WKST={v} is not supported: weeks start on MO"));
                    }
                }
                other => {
                    return Err(format!(
                        "RRULE part {other} is not supported: use FREQ, INTERVAL, BYDAY \
                         (weekly), BYMONTHDAY (monthly), UNTIL, or COUNT"
                    ));
                }
            }
        }
        rule.freq = freq.ok_or("the RRULE has no FREQ")?;
        rule.check()?;
        Ok(rule.normalized())
    }

    /// The rule as RRULE text, for display and for other calendars.
    pub fn to_rrule(&self) -> String {
        let mut s = format!("FREQ={}", self.freq.as_str().to_ascii_uppercase());
        if self.interval != 1 {
            s.push_str(&format!(";INTERVAL={}", self.interval));
        }
        if !self.weekdays.is_empty() {
            let days: Vec<String> = self
                .weekdays
                .iter()
                .map(|d| d.code().to_ascii_uppercase())
                .collect();
            s.push_str(&format!(";BYDAY={}", days.join(",")));
        }
        if let Some(d) = self.month_day {
            s.push_str(&format!(";BYMONTHDAY={d}"));
        }
        match self.until.as_deref().map(parse_until) {
            Some(Ok(Until::Date(d))) => s.push_str(&format!(";UNTIL={}", d.format("%Y%m%d"))),
            Some(Ok(Until::Instant(t))) => {
                s.push_str(&format!(";UNTIL={}", t.format("%Y%m%dT%H%M%SZ")))
            }
            _ => {}
        }
        if let Some(c) = self.count {
            s.push_str(&format!(";COUNT={c}"));
        }
        s
    }

    /// The rule in a few words: `every 2 weeks on mo, we, 6 times`.
    pub fn describe(&self) -> String {
        let unit = match self.freq {
            Freq::Daily => "day",
            Freq::Weekly => "week",
            Freq::Monthly => "month",
        };
        let mut s = if self.interval == 1 {
            format!("every {unit}")
        } else {
            format!("every {} {unit}s", self.interval)
        };
        if !self.weekdays.is_empty() {
            let d: Vec<&str> = self.weekdays.iter().map(|d| d.code()).collect();
            s.push_str(&format!(" on {}", d.join(", ")));
        }
        match self.month_day {
            Some(-1) => s.push_str(" on the last day"),
            Some(d) => s.push_str(&format!(" on day {d}")),
            None => {}
        }
        if let Some(u) = &self.until {
            s.push_str(&format!(" until {u}"));
        }
        if let Some(c) = self.count {
            s.push_str(&format!(", {c} times"));
        }
        if let Some(z) = &self.tz {
            s.push_str(&format!(" ({z})"));
        }
        s
    }
}

/// `20261231` or `20261231T235959Z` as the stored `until`.
fn rrule_until(v: &str) -> Result<String, String> {
    if let Ok(d) = NaiveDate::parse_from_str(v, "%Y%m%d") {
        return Ok(d.format("%Y-%m-%d").to_string());
    }
    if let Some(z) = v.strip_suffix('Z')
        && let Ok(t) = NaiveDateTime::parse_from_str(z, "%Y%m%dT%H%M%S")
    {
        return Ok(crate::time_param::stored(Utc.from_utc_datetime(&t)));
    }
    Err(format!(
        "bad UNTIL `{v}`: use 20261231 or 20261231T235959Z (UTC)"
    ))
}

/// The first instant of local date-time `ndt` in `tz`: a skipped local
/// time (the spring gap) moves on by the gap, a doubled one (the autumn
/// overlap) takes the first.
pub fn local_instant(tz: &Tz, ndt: NaiveDateTime) -> DateTime<Utc> {
    for step in 0..=24 {
        let probe = ndt + Duration::minutes(30 * step);
        match tz.from_local_datetime(&probe) {
            LocalResult::Single(t) => return t.with_timezone(&Utc),
            LocalResult::Ambiguous(a, _) => return a.with_timezone(&Utc),
            LocalResult::None => continue,
        }
    }
    Utc.from_utc_datetime(&ndt)
}

/// One occurrence: the start is its key (an exception names it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Occurrence {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

/// The shape of an event for an expansion.
#[derive(Debug, Clone)]
pub struct Series<'a> {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub all_day: bool,
    /// The zone of the event.
    pub tz: &'a str,
    pub rule: Option<&'a Rule>,
    /// The starts of the skipped occurrences.
    pub skipped: &'a BTreeSet<DateTime<Utc>>,
}

/// Does `o` touch the window `[from, to)`? An occurrence of no length
/// touches it when it starts in it.
fn touches(o: &Occurrence, from: DateTime<Utc>, to: DateTime<Utc>) -> bool {
    o.start < to && (o.end > from || o.start >= from)
}

fn add_months(y: i32, m: u32, n: i64) -> (i32, u32) {
    let idx = i64::from(y) * 12 + i64::from(m) - 1 + n;
    (idx.div_euclid(12) as i32, (idx.rem_euclid(12) + 1) as u32)
}

fn days_in_month(y: i32, m: u32) -> u32 {
    let (ny, nm) = add_months(y, m, 1);
    NaiveDate::from_ymd_opt(ny, nm, 1)
        .and_then(|d| d.pred_opt())
        .map_or(28, |d| d.day())
}

impl Series<'_> {
    /// The occurrences that touch `[from, to)`, oldest first, at most
    /// `limit`. Skipped occurrences are not in the list. The second value
    /// is true when the limit cut the list.
    pub fn expand(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        limit: usize,
    ) -> (Vec<Occurrence>, bool) {
        let mut out = Vec::new();
        let mut cut = false;
        self.each(
            |o| {
                if o.start >= to {
                    return false;
                }
                if touches(&o, from, to) && !self.skipped.contains(&o.start) {
                    if out.len() >= limit {
                        cut = true;
                        return false;
                    }
                    out.push(o);
                }
                true
            },
            from,
        );
        (out, cut)
    }

    /// Is `start` the start of an occurrence (skipped or not)?
    pub fn has_occurrence(&self, start: DateTime<Utc>) -> bool {
        let mut found = false;
        self.each(
            |o| {
                if o.start == start {
                    found = true;
                }
                o.start < start
            },
            start,
        );
        found
    }

    /// The occurrence (not skipped) that runs at `at`, if any: the last
    /// one that started at or before it and ends after it.
    pub fn running_at(&self, at: DateTime<Utc>) -> Option<Occurrence> {
        let (list, _) = self.expand(at, at + Duration::milliseconds(1), 1000);
        list.into_iter()
            .rfind(|o| o.start <= at && (o.end > at || o.end == o.start && o.start == at))
    }

    /// Call `f` with each occurrence in order, skipped ones too, until it
    /// returns false. `hint` is a time before which no occurrence matters:
    /// a rule with no `count` jumps near it.
    fn each(&self, mut f: impl FnMut(Occurrence) -> bool, hint: DateTime<Utc>) {
        let Some(rule) = self.rule else {
            f(Occurrence {
                start: self.start,
                end: self.end,
            });
            return;
        };
        let tz = zone(rule.tz.as_deref().unwrap_or(self.tz)).unwrap_or(Tz::UTC);
        let local = self.start.with_timezone(&tz);
        let date0 = local.date_naive();
        let time0 = if self.all_day {
            NaiveTime::MIN
        } else {
            local.time()
        };
        // An all-day event lasts whole local days; a timed one a fixed
        // length.
        let ezone = zone(self.tz).unwrap_or(Tz::UTC);
        let days = if self.all_day {
            let a = self.start.with_timezone(&ezone).date_naive();
            let b = self.end.with_timezone(&ezone).date_naive();
            (b - a).num_days().max(1)
        } else {
            0
        };
        let length = self.end - self.start;
        let until = rule.until.as_deref().and_then(|u| parse_until(u).ok());
        let make = |d: NaiveDate| -> Occurrence {
            let start = local_instant(&tz, d.and_time(time0));
            let end = if self.all_day {
                local_instant(&tz, (d + Duration::days(days)).and_time(NaiveTime::MIN))
            } else {
                start + length
            };
            Occurrence { start, end }
        };
        let interval = i64::from(rule.interval);
        // The first period to look at: with no count, jump to a little
        // before the hint.
        let hint_date = (hint - length - Duration::days(days + 2))
            .with_timezone(&tz)
            .date_naive();
        let lead_days = (hint_date - date0).num_days();
        let mut k: i64 = 0;
        if rule.count.is_none() && lead_days > 0 {
            k = match rule.freq {
                Freq::Daily => lead_days / interval,
                Freq::Weekly => lead_days / (7 * interval),
                Freq::Monthly => lead_days / (31 * interval),
            }
            .saturating_sub(1)
            .max(0);
        }
        let week0 = date0 - Duration::days(i64::from(date0.weekday().num_days_from_monday()));
        let weekdays: Vec<Day> = if rule.weekdays.is_empty() {
            vec![Day::of(date0.weekday())]
        } else {
            rule.weekdays.clone()
        };
        let mut made: u32 = 0;
        let mut steps = 0usize;
        loop {
            steps += 1;
            if steps > MAX_STEPS {
                return;
            }
            let dates: Vec<NaiveDate> = match rule.freq {
                Freq::Daily => vec![date0 + Duration::days(k * interval)],
                Freq::Weekly => {
                    let week = week0 + Duration::days(7 * k * interval);
                    weekdays
                        .iter()
                        .map(|d| week + Duration::days(i64::from(d.0)))
                        .filter(|d| *d >= date0)
                        .collect()
                }
                Freq::Monthly => {
                    let (y, m) = add_months(date0.year(), date0.month(), k * interval);
                    let want = rule.month_day.unwrap_or(date0.day() as i32);
                    let dim = days_in_month(y, m);
                    let day = if want == -1 { dim } else { want as u32 };
                    match NaiveDate::from_ymd_opt(y, m, day) {
                        Some(d) if day <= dim && d >= date0 => vec![d],
                        _ => vec![],
                    }
                }
            };
            k += 1;
            for d in dates {
                if let Some(Until::Date(u)) = until
                    && d > u
                {
                    return;
                }
                let o = make(d);
                if let Some(Until::Instant(u)) = until
                    && o.start > u
                {
                    return;
                }
                made += 1;
                if rule.count.is_some_and(|c| made > c) {
                    return;
                }
                if !f(o) {
                    return;
                }
            }
            if k > 1_000_000 {
                return;
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn rule(v: serde_json::Value) -> Rule {
        let r: Rule = serde_json::from_value(v).unwrap();
        r.check().unwrap();
        r.normalized()
    }

    fn starts(s: &Series<'_>, from: &str, to: &str) -> Vec<String> {
        s.expand(t(from), t(to), 1000)
            .0
            .iter()
            .map(|o| crate::time_param::stored(o.start))
            .collect()
    }

    #[test]
    fn rrule_round_trip_and_refusals() {
        let r = Rule::parse_rrule("RRULE:FREQ=WEEKLY;INTERVAL=2;BYDAY=WE,MO;COUNT=6").unwrap();
        assert_eq!(r.freq, Freq::Weekly);
        assert_eq!(r.interval, 2);
        assert_eq!(r.weekdays, vec![Day(0), Day(2)]);
        assert_eq!(r.count, Some(6));
        assert_eq!(r.to_rrule(), "FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE;COUNT=6");
        let u = Rule::parse_rrule("FREQ=MONTHLY;BYMONTHDAY=-1;UNTIL=20261231").unwrap();
        assert_eq!(u.until.as_deref(), Some("2026-12-31"));
        assert_eq!(u.to_rrule(), "FREQ=MONTHLY;BYMONTHDAY=-1;UNTIL=20261231");
        let z = Rule::parse_rrule("FREQ=DAILY;UNTIL=20261231T120000Z").unwrap();
        assert_eq!(z.until.as_deref(), Some("2026-12-31T12:00:00.000Z"));
        assert_eq!(z.to_rrule(), "FREQ=DAILY;UNTIL=20261231T120000Z");
        for (bad, word) in [
            ("FREQ=YEARLY", "YEARLY"),
            ("FREQ=WEEKLY;BYDAY=1MO", "BYDAY"),
            ("FREQ=MONTHLY;BYSETPOS=1", "BYSETPOS"),
            ("FREQ=DAILY;BYHOUR=9", "BYHOUR"),
            ("FREQ=DAILY;COUNT=2;UNTIL=20261231", "not both"),
            ("FREQ=DAILY;BYDAY=MO", "weekdays"),
            ("FREQ=WEEKLY;BYMONTHDAY=3", "month_day"),
            ("FREQ=MONTHLY;BYMONTHDAY=1,15", "more than one"),
            ("FREQ=WEEKLY;WKST=SU", "WKST"),
            ("INTERVAL=2", "no FREQ"),
            ("FREQ=DAILY;INTERVAL=0", "interval"),
        ] {
            let e = Rule::parse_rrule(bad).unwrap_err();
            assert!(e.contains(word), "{bad}: {e}");
        }
    }

    #[test]
    fn json_rules_check_their_fields() {
        let bad = |v: serde_json::Value| {
            let r: Rule = serde_json::from_value(v).unwrap();
            r.check().unwrap_err()
        };
        assert!(bad(serde_json::json!({"freq": "daily", "tz": "Mars/Base"})).contains("tz"));
        assert!(bad(serde_json::json!({"freq": "monthly", "month_day": 32})).contains("month_day"));
        assert!(bad(serde_json::json!({"freq": "daily", "until": "soon"})).contains("until"));
        assert!(
            serde_json::from_value::<Rule>(
                serde_json::json!({"freq": "weekly", "weekdays": ["xx"]})
            )
            .is_err()
        );
        assert!(serde_json::from_value::<Rule>(serde_json::json!({"freq": "yearly"})).is_err());
        let r = rule(serde_json::json!({"freq": "weekly", "weekdays": ["fri", "Monday", "mo"]}));
        assert_eq!(r.weekdays, vec![Day(0), Day(4)]);
        assert_eq!(r.describe(), "every week on mo, fr");
    }

    #[test]
    fn a_single_event_and_the_window() {
        let none = BTreeSet::new();
        let s = Series {
            start: t("2026-11-10T09:00:00Z"),
            end: t("2026-11-10T10:00:00Z"),
            all_day: false,
            tz: "UTC",
            rule: None,
            skipped: &none,
        };
        assert_eq!(
            starts(&s, "2026-11-10T09:30:00Z", "2026-11-11T00:00:00Z").len(),
            1
        );
        assert!(starts(&s, "2026-11-10T10:00:00Z", "2026-11-11T00:00:00Z").is_empty());
        assert!(starts(&s, "2026-11-09T00:00:00Z", "2026-11-10T09:00:00Z").is_empty());
        // A point in time touches the window it starts in.
        let p = Series {
            end: t("2026-11-10T09:00:00Z"),
            ..s.clone()
        };
        assert_eq!(
            starts(&p, "2026-11-10T09:00:00Z", "2026-11-10T09:00:01Z").len(),
            1
        );
    }

    #[test]
    fn daily_weekly_monthly_with_ends_and_skips() {
        let r = rule(serde_json::json!({"freq": "daily", "interval": 2, "count": 3}));
        let skipped = BTreeSet::new();
        let s = Series {
            start: t("2026-11-01T22:00:00Z"),
            end: t("2026-11-01T23:00:00Z"),
            all_day: false,
            tz: "UTC",
            rule: Some(&r),
            skipped: &skipped,
        };
        assert_eq!(
            starts(&s, "2026-10-01T00:00:00Z", "2027-01-01T00:00:00Z"),
            [
                "2026-11-01T22:00:00.000Z",
                "2026-11-03T22:00:00.000Z",
                "2026-11-05T22:00:00.000Z"
            ]
        );
        // A skip keeps its place in the count.
        let mut skipped2 = skipped.clone();
        skipped2.insert(t("2026-11-03T22:00:00Z"));
        let s2 = Series {
            skipped: &skipped2,
            ..s.clone()
        };
        assert_eq!(
            starts(&s2, "2026-10-01T00:00:00Z", "2027-01-01T00:00:00Z"),
            ["2026-11-01T22:00:00.000Z", "2026-11-05T22:00:00.000Z"]
        );
        assert!(s2.has_occurrence(t("2026-11-03T22:00:00Z")));
        assert!(!s2.has_occurrence(t("2026-11-04T22:00:00Z")));

        // Weekly on Monday and Friday from a Wednesday: the first is Friday.
        let none = BTreeSet::new();
        let w = rule(
            serde_json::json!({"freq": "weekly", "weekdays": ["mo", "fr"],
                                         "until": "2026-11-16"}),
        );
        let s = Series {
            start: t("2026-11-04T08:00:00Z"),
            end: t("2026-11-04T08:30:00Z"),
            all_day: false,
            tz: "UTC",
            rule: Some(&w),
            skipped: &none,
        };
        assert_eq!(
            starts(&s, "2026-11-01T00:00:00Z", "2027-01-01T00:00:00Z"),
            [
                "2026-11-06T08:00:00.000Z",
                "2026-11-09T08:00:00.000Z",
                "2026-11-13T08:00:00.000Z",
                "2026-11-16T08:00:00.000Z"
            ]
        );

        // Monthly on the 31st skips the short months; -1 is the last day.
        let m = rule(serde_json::json!({"freq": "monthly", "count": 3}));
        let s = Series {
            start: t("2027-01-31T12:00:00Z"),
            end: t("2027-01-31T13:00:00Z"),
            all_day: false,
            tz: "UTC",
            rule: Some(&m),
            skipped: &none,
        };
        assert_eq!(
            starts(&s, "2027-01-01T00:00:00Z", "2028-01-01T00:00:00Z"),
            [
                "2027-01-31T12:00:00.000Z",
                "2027-03-31T12:00:00.000Z",
                "2027-05-31T12:00:00.000Z"
            ]
        );
        let last = rule(serde_json::json!({"freq": "monthly", "month_day": -1, "count": 3}));
        let s = Series {
            rule: Some(&last),
            ..s
        };
        assert_eq!(
            starts(&s, "2027-01-01T00:00:00Z", "2028-01-01T00:00:00Z"),
            [
                "2027-01-31T12:00:00.000Z",
                "2027-02-28T12:00:00.000Z",
                "2027-03-31T12:00:00.000Z"
            ]
        );
    }

    #[test]
    fn a_weekly_rule_keeps_the_local_time_across_dst() {
        // 09:00 in London each Sunday: 09:00Z in winter, 08:00Z in summer
        // (the change is on 2027-03-28).
        let none = BTreeSet::new();
        let r = rule(serde_json::json!({"freq": "weekly"}));
        let s = Series {
            start: t("2027-03-14T09:00:00Z"),
            end: t("2027-03-14T10:00:00Z"),
            all_day: false,
            tz: "Europe/London",
            rule: Some(&r),
            skipped: &none,
        };
        assert_eq!(
            starts(&s, "2027-03-14T00:00:00Z", "2027-04-05T00:00:00Z"),
            [
                "2027-03-14T09:00:00.000Z",
                "2027-03-21T09:00:00.000Z",
                "2027-03-28T08:00:00.000Z",
                "2027-04-04T08:00:00.000Z"
            ]
        );
        // The length stays one hour.
        let (list, _) = s.expand(t("2027-03-28T00:00:00Z"), t("2027-03-29T00:00:00Z"), 10);
        assert_eq!(list[0].end - list[0].start, Duration::hours(1));
        // The rule zone wins over the event zone.
        let ny = rule(serde_json::json!({"freq": "daily", "tz": "America/New_York"}));
        let s = Series {
            start: t("2027-03-13T14:00:00Z"),
            end: t("2027-03-13T15:00:00Z"),
            tz: "UTC",
            rule: Some(&ny),
            ..s
        };
        assert_eq!(
            starts(&s, "2027-03-13T00:00:00Z", "2027-03-16T00:00:00Z"),
            [
                "2027-03-13T14:00:00.000Z",
                "2027-03-14T13:00:00.000Z",
                "2027-03-15T13:00:00.000Z"
            ]
        );
    }

    #[test]
    fn the_spring_gap_moves_on_and_the_autumn_overlap_takes_the_first() {
        let none = BTreeSet::new();
        let r = rule(serde_json::json!({"freq": "daily"}));
        // 01:30 London: on 2027-03-28 01:30 does not exist (01:00 -> 02:00).
        let s = Series {
            start: t("2027-03-27T01:30:00Z"),
            end: t("2027-03-27T01:45:00Z"),
            all_day: false,
            tz: "Europe/London",
            rule: Some(&r),
            skipped: &none,
        };
        assert_eq!(
            starts(&s, "2027-03-27T00:00:00Z", "2027-03-30T00:00:00Z"),
            [
                "2027-03-27T01:30:00.000Z",
                "2027-03-28T01:00:00.000Z",
                "2027-03-29T00:30:00.000Z"
            ]
        );
        // On 2027-10-31 01:30 comes twice: the first is 00:30Z (BST).
        let s = Series {
            start: t("2027-10-30T00:30:00Z"),
            end: t("2027-10-30T00:45:00Z"),
            ..s
        };
        assert_eq!(
            starts(&s, "2027-10-31T00:00:00Z", "2027-11-01T00:00:00Z"),
            ["2027-10-31T00:30:00.000Z"]
        );
    }

    #[test]
    fn all_day_events_last_whole_local_days() {
        let none = BTreeSet::new();
        let r = rule(serde_json::json!({"freq": "weekly", "count": 3}));
        // A two-day freeze from local midnight in Sydney.
        let tz = zone("Australia/Sydney").unwrap();
        let start = local_instant(
            &tz,
            NaiveDate::from_ymd_opt(2026, 9, 26)
                .unwrap()
                .and_time(NaiveTime::MIN),
        );
        let end = local_instant(
            &tz,
            NaiveDate::from_ymd_opt(2026, 9, 28)
                .unwrap()
                .and_time(NaiveTime::MIN),
        );
        let s = Series {
            start,
            end,
            all_day: true,
            tz: "Australia/Sydney",
            rule: Some(&r),
            skipped: &none,
        };
        let (list, _) = s.expand(start, start + Duration::days(30), 10);
        assert_eq!(list.len(), 3);
        // Sydney moves to DST on 2026-10-04: the occurrence of 10-03 is 47 h.
        for o in &list {
            let a = o.start.with_timezone(&tz);
            let b = o.end.with_timezone(&tz);
            assert_eq!(a.time(), NaiveTime::MIN);
            assert_eq!(b.time(), NaiveTime::MIN);
            assert_eq!((b.date_naive() - a.date_naive()).num_days(), 2);
        }
        assert_eq!(list[1].end - list[1].start, Duration::hours(47));
    }

    #[test]
    fn a_far_window_jumps_and_the_limit_cuts() {
        let none = BTreeSet::new();
        let r = rule(serde_json::json!({"freq": "daily"}));
        let s = Series {
            start: t("2020-01-01T06:00:00Z"),
            end: t("2020-01-01T07:00:00Z"),
            all_day: false,
            tz: "UTC",
            rule: Some(&r),
            skipped: &none,
        };
        assert_eq!(
            starts(&s, "2026-11-10T00:00:00Z", "2026-11-12T00:00:00Z"),
            ["2026-11-10T06:00:00.000Z", "2026-11-11T06:00:00.000Z"]
        );
        let (list, cut) = s.expand(t("2026-01-01T00:00:00Z"), t("2027-01-01T00:00:00Z"), 5);
        assert_eq!(list.len(), 5);
        assert!(cut);
        let o = s.running_at(t("2026-11-10T06:30:00Z")).unwrap();
        assert_eq!(o.start, t("2026-11-10T06:00:00Z"));
        assert!(s.running_at(t("2026-11-10T07:30:00Z")).is_none());
    }
}
