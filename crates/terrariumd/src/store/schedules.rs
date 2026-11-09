//! Schedules (docs/api.md §19): the rows, the time math, and the run log.
//!
//! A schedule runs one action (mail, a turn, an exec, an agent verb, or a
//! quota probe) on a cron, interval, or one-time spec. It runs as the
//! actor in `run_as`, and the daemon checks that actor's rights again at
//! each run (`crate::api::schedules`). The tick that finds the due
//! schedules is `crate::scheduler`.
//!
//! A trigger is a schedule whose spec is `on` (PLAN §12c): an event of the
//! log starts its run. Its rows are the same; `trigger_state` keeps its
//! dedupe, escalation, and rate counters, and `trigger_cursor` the last
//! event that the trigger engine (`crate::scheduler`) read.
//!
//! Times are stored as RFC 3339 UTC with milliseconds ([`stamp`]), so a
//! text compare orders them.

use std::str::FromStr;

use chrono::{DateTime, Datelike, NaiveDate, NaiveTime, SecondsFormat, TimeZone, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use terrarium_core::schedule::{
    CardOp, CatchUp, Escalate, ScheduleAction, ScheduleDef, ScheduleSpec,
};
use terrarium_core::template;

use crate::db::Tx;
use crate::error::{ApiError, ApiResult, Code};
use crate::events::EventDraft;
use crate::owner::Owner;
use crate::store::mail::{self, Address, Outgoing};
use crate::store::{Page, Paged, agents, board, projects, subscriptions, targets, valid_name};

/// The shortest interval between two runs, in seconds. A five-field cron
/// expression cannot go under it.
pub const MIN_INTERVAL_SECS: i64 = 60;
/// The run limit of an `exec` action without `timeout_secs`.
pub const DEFAULT_EXEC_TIMEOUT: u64 = 300;
/// The highest `timeout_secs` of an `exec` action.
pub const MAX_EXEC_TIMEOUT: u64 = 3600;
/// The runs of each schedule that the daily prune keeps.
pub const KEEP_RUNS: usize = 500;
/// The rights checks in a row that skip a run before the schedule is
/// disabled.
pub const MAX_SKIPS: i64 = 3;
/// `TERRARIUM_MAX_SCHEDULES_PER_PROJECT`: the most schedules of one
/// project. A create past it is `409 conflict`.
pub const ENV_MAX_SCHEDULES: &str = "TERRARIUM_MAX_SCHEDULES_PER_PROJECT";
/// The default of [`ENV_MAX_SCHEDULES`].
pub const DEFAULT_MAX_SCHEDULES: usize = 20;
/// The highest `max_per_hour` of a trigger.
pub const MAX_PER_HOUR_LIMIT: u32 = 600;
/// The longest dedupe window of a trigger, in seconds (7 days).
pub const MAX_DEDUPE_WINDOW: i64 = 7 * 86_400;

/// The cap: [`ENV_MAX_SCHEDULES`], else [`DEFAULT_MAX_SCHEDULES`]. Read
/// once.
pub fn max_per_project() -> usize {
    static MAX: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *MAX.get_or_init(|| {
        std::env::var(ENV_MAX_SCHEDULES)
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(DEFAULT_MAX_SCHEDULES)
    })
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

/// A time as stored.
pub fn stamp(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// A `422 invalid` that names the field: the message starts with it, and
/// `details.field` holds it.
pub fn field_error(field: &str, msg: impl std::fmt::Display) -> ApiError {
    ApiError::invalid(format!("{field}: {msg}")).with_details(json!({"field": field}))
}

// ---------------------------------------------------------------- the spec

/// Standard cron numbers the days of the week 0-7 (0 and 7 are Sunday);
/// the `cron` crate numbers them 1-7 from Sunday. Names stay as they are.
fn dow_number(s: &str) -> Result<Option<u32>, String> {
    if s.chars().all(|c| c.is_ascii_digit()) && !s.is_empty() {
        let n: u32 = s.parse().map_err(|_| format!("bad day `{s}`"))?;
        if n > 7 {
            return Err(format!("day of week {n} is not 0-7"));
        }
        return Ok(Some(if n == 0 || n == 7 { 1 } else { n + 1 }));
    }
    Ok(None)
}

fn convert_dow(field: &str) -> Result<String, String> {
    let mut out = Vec::new();
    for item in field.split(',') {
        let (base, step) = match item.split_once('/') {
            Some((b, s)) => (b, Some(s)),
            None => (item, None),
        };
        let step_txt = step.map(|s| format!("/{s}")).unwrap_or_default();
        if base == "*" || base == "?" {
            out.push(format!("{base}{step_txt}"));
            continue;
        }
        match base.split_once('-') {
            Some((a, b)) => {
                let na = dow_number(a)?;
                let raw_b: Option<u32> = b.parse().ok();
                let nb = dow_number(b)?;
                let a_txt = na.map_or_else(|| a.to_owned(), |n| n.to_string());
                if raw_b == Some(7) && na.is_some() {
                    // `5-7` is Friday to Sunday: Sunday is 1 in the crate.
                    let start = na.unwrap_or(1);
                    if start == 1 {
                        out.push(format!("1-7{step_txt}"));
                    } else if step.is_some() {
                        return Err(format!(
                            "write `{item}` without 7 (Sunday is 0), for example `{a}-6/..,0`"
                        ));
                    } else {
                        out.push(format!("{start}-7"));
                        out.push("1".into());
                    }
                    continue;
                }
                let b_txt = nb.map_or_else(|| b.to_owned(), |n| n.to_string());
                out.push(format!("{a_txt}-{b_txt}{step_txt}"));
            }
            None => {
                let n = dow_number(base)?;
                out.push(format!(
                    "{}{step_txt}",
                    n.map_or_else(|| base.to_owned(), |n| n.to_string())
                ));
            }
        }
    }
    Ok(out.join(","))
}

/// Parse a five-field cron expression, or a shorthand (`@hourly`,
/// `@daily`, `@weekly`, `@monthly`, `@yearly`). The error says why.
pub fn parse_cron(expr: &str) -> Result<cron::Schedule, String> {
    let expr = expr.trim();
    if expr.starts_with('@') {
        return match expr {
            "@hourly" | "@daily" | "@weekly" | "@monthly" | "@yearly" => {
                cron::Schedule::from_str(expr).map_err(|e| e.to_string())
            }
            "@midnight" => cron::Schedule::from_str("@daily").map_err(|e| e.to_string()),
            "@annually" => cron::Schedule::from_str("@yearly").map_err(|e| e.to_string()),
            _ => Err("the shorthands are @hourly, @daily, @weekly, @monthly, and @yearly".into()),
        };
    }
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(format!(
            "use five fields (minute hour day-of-month month day-of-week), not {}",
            fields.len()
        ));
    }
    let dow = convert_dow(fields[4])?;
    let full = format!(
        "0 {} {} {} {} {}",
        fields[0], fields[1], fields[2], fields[3], dow
    );
    cron::Schedule::from_str(&full).map_err(|e| e.to_string())
}

/// Parse an IANA zone (`UTC`, `Europe/London`).
pub fn parse_tz(tz: &str) -> Result<chrono_tz::Tz, String> {
    chrono_tz::Tz::from_str(tz.trim()).map_err(|_| format!("unknown time zone `{tz}`"))
}

/// Parse an interval: numbers with `s`, `m`, `h`, or `d`, for example
/// `90s`, `30m`, `1h30m`. Returns seconds.
pub fn parse_every(s: &str) -> Result<i64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("the interval is empty".into());
    }
    let mut total: i64 = 0;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            _ => return Err(format!("bad interval `{s}`: use s, m, h, or d, as `30m`")),
        };
        let n: i64 = num
            .parse()
            .map_err(|_| format!("bad interval `{s}`: a number goes before each unit"))?;
        total = total.saturating_add(n.saturating_mul(unit));
        num.clear();
    }
    if !num.is_empty() {
        return Err(format!("bad interval `{s}`: give a unit, as `{num}m`"));
    }
    Ok(total)
}

/// Check a spec. `now` is the time of the write: an `at` before it is an
/// error.
pub fn validate_spec(spec: &ScheduleSpec, now: DateTime<Utc>) -> ApiResult<()> {
    match spec {
        ScheduleSpec::Cron { expr, tz } => {
            parse_cron(expr).map_err(|e| {
                field_error("spec.expr", format!("bad cron expression `{expr}`: {e}"))
            })?;
            parse_tz(tz).map_err(|e| field_error("spec.tz", e))?;
        }
        ScheduleSpec::Interval { every } => {
            let secs = parse_every(every).map_err(|e| field_error("spec.every", e))?;
            if secs < MIN_INTERVAL_SECS {
                return Err(field_error(
                    "spec.every",
                    format!("`{every}` is under the shortest interval, {MIN_INTERVAL_SECS} s"),
                ));
            }
        }
        ScheduleSpec::At { at } => {
            let t = parse_time(at)
                .ok_or_else(|| field_error("spec.at", format!("`{at}` is not an RFC 3339 time")))?;
            if t <= now {
                return Err(field_error("spec.at", format!("`{at}` is in the past")));
            }
        }
        ScheduleSpec::On {
            kinds,
            matches,
            dedupe,
            escalate,
            max_per_hour,
        } => validate_on(
            kinds,
            matches,
            dedupe.as_ref(),
            escalate.as_ref(),
            *max_per_hour,
        )?,
        // A project action: nothing to check.
        ScheduleSpec::Manual {} => {}
        ScheduleSpec::DayStart {
            tz,
            not_before,
            not_after,
            delay,
            weekdays,
        } => {
            parse_tz(tz).map_err(|e| field_error("spec.tz", e))?;
            let nb = not_before
                .as_deref()
                .map(parse_hhmm)
                .transpose()
                .map_err(|e| field_error("spec.not_before", e))?;
            let na = not_after
                .as_deref()
                .map(parse_hhmm)
                .transpose()
                .map_err(|e| field_error("spec.not_after", e))?;
            if let (Some(b), Some(a)) = (nb, na)
                && b > a
            {
                return Err(field_error(
                    "spec.not_after",
                    "`not_after` is before `not_before`",
                ));
            }
            if let Some(d) = delay {
                let secs = parse_every(d).map_err(|e| field_error("spec.delay", e))?;
                if secs > MAX_DAY_START_DELAY {
                    return Err(field_error(
                        "spec.delay",
                        format!("`{d}` is over the longest delay, 12h"),
                    ));
                }
            }
            for w in weekdays {
                parse_weekday(w).map_err(|e| field_error("spec.weekdays", e))?;
            }
        }
    }
    Ok(())
}

fn check_globs(field: &str, globs: &[String]) -> ApiResult<()> {
    for g in globs {
        if g.trim().is_empty() || g.chars().any(char::is_whitespace) {
            return Err(field_error(
                field,
                format!("bad kind glob `{g}`: use a kind such as `ci.run.failed` or `ci.job.*`"),
            ));
        }
    }
    Ok(())
}

fn check_template(field: &str, text: &str) -> ApiResult<()> {
    template::check(text).map_err(|e| field_error(field, e))
}

fn validate_on(
    kinds: &[String],
    matches: &[terrarium_core::schedule::MatchRule],
    dedupe: Option<&terrarium_core::schedule::Dedupe>,
    escalate: Option<&Escalate>,
    max_per_hour: u32,
) -> ApiResult<()> {
    if kinds.is_empty() {
        return Err(field_error(
            "spec.kinds",
            "name one or more event kinds, for example [\"ci.run.failed\"]",
        ));
    }
    check_globs("spec.kinds", kinds)?;
    for m in matches {
        m.check().map_err(|e| field_error("spec.match", e))?;
    }
    if let Some(d) = dedupe {
        if d.key.trim().is_empty() {
            return Err(field_error("spec.dedupe.key", "the dedupe key is empty"));
        }
        check_template("spec.dedupe.key", &d.key)?;
        let secs = parse_every(&d.window).map_err(|e| field_error("spec.dedupe.window", e))?;
        if !(1..=MAX_DEDUPE_WINDOW).contains(&secs) {
            return Err(field_error(
                "spec.dedupe.window",
                format!("`{}` is not 1 s to 7 d", d.window),
            ));
        }
    }
    if let Some(e) = escalate {
        if e.key.trim().is_empty() {
            return Err(field_error(
                "spec.escalate.key",
                "the escalation key is empty",
            ));
        }
        check_template("spec.escalate.key", &e.key)?;
        if e.after == 0 {
            return Err(field_error("spec.escalate.after", "use 1 or more"));
        }
        check_globs("spec.escalate.reset_on", &e.reset_on)?;
        if e.to.trim().is_empty() {
            return Err(field_error("spec.escalate.to", "name an address"));
        }
    }
    if max_per_hour == 0 || max_per_hour > MAX_PER_HOUR_LIMIT {
        return Err(field_error(
            "spec.max_per_hour",
            format!("{max_per_hour} is not 1-{MAX_PER_HOUR_LIMIT}"),
        ));
    }
    Ok(())
}

/// The checks of a spec with its action, at each write (the API, a
/// project file, a fleet): a trigger takes only `mail`, `exec`, and
/// `card` (no `turn`: a trigger notifies an agent by mail), its templates
/// parse, and its escalation address resolves.
pub fn check_trigger(
    conn: &Connection,
    project: Option<&str>,
    spec: &ScheduleSpec,
    action: &ScheduleAction,
) -> ApiResult<()> {
    check_trigger_shape(spec, action)?;
    if let ScheduleSpec::On {
        escalate: Some(e), ..
    } = spec
    {
        escalation_reach(conn, project, e)?;
    }
    Ok(())
}

/// The part of [`check_trigger`] that reads no store: the action kind and
/// the templates (a fleet check, before the fleet's agents exist).
pub fn check_trigger_shape(spec: &ScheduleSpec, action: &ScheduleAction) -> ApiResult<()> {
    if !spec.is_trigger() {
        return Ok(());
    }
    if !action.allowed_on_trigger() {
        return Err(field_error(
            "action",
            format!(
                "a trigger (spec `on`) runs a mail, exec, card, or webhook action, not `{}`; \
                 to notify an agent, mail it",
                action.kind()
            ),
        ));
    }
    for (field, text) in action.templated() {
        check_template(field, text)?;
    }
    Ok(())
}

/// The address of an escalation, as a mail reach.
pub fn escalation_reach(
    conn: &Connection,
    project: Option<&str>,
    e: &Escalate,
) -> ApiResult<Reach> {
    let a = mail::resolve(conn, &e.to, project)
        .map_err(|err| not_found_as_field(err, "spec.escalate.to"))?;
    if a == Address::System {
        return Err(field_error("spec.escalate.to", "`system` only sends mail"));
    }
    Ok(Reach::Mail(a))
}

/// The first run of `spec` after `after`. `anchor` is the start of an
/// interval: its runs are `anchor + k * every`, k >= 1. `None`: no more
/// runs (an `at` in the past).
pub fn next_after(
    spec: &ScheduleSpec,
    anchor: DateTime<Utc>,
    after: DateTime<Utc>,
) -> ApiResult<Option<DateTime<Utc>>> {
    next_after_min(spec, anchor, after, MIN_INTERVAL_SECS)
}

/// [`next_after`] with another shortest interval: a monitor
/// (`crate::store::monitors`) runs as often as every 15 s.
pub fn next_after_min(
    spec: &ScheduleSpec,
    anchor: DateTime<Utc>,
    after: DateTime<Utc>,
    min_secs: i64,
) -> ApiResult<Option<DateTime<Utc>>> {
    match spec {
        ScheduleSpec::Cron { expr, tz } => {
            let sched = parse_cron(expr).map_err(|e| {
                field_error("spec.expr", format!("bad cron expression `{expr}`: {e}"))
            })?;
            let zone = parse_tz(tz).map_err(|e| field_error("spec.tz", e))?;
            let local = zone.from_utc_datetime(&after.naive_utc());
            Ok(sched.after(&local).next().map(|t| t.with_timezone(&Utc)))
        }
        ScheduleSpec::Interval { every } => {
            let secs = parse_every(every)
                .map_err(|e| field_error("spec.every", e))?
                .max(min_secs.max(1));
            let gone = (after - anchor).num_milliseconds();
            let step = secs * 1000;
            let k = if gone < 0 { 1 } else { gone / step + 1 };
            Ok(Some(anchor + chrono::Duration::milliseconds(k * step)))
        }
        ScheduleSpec::At { at } => Ok(parse_time(at).filter(|t| *t > after)),
        // A trigger has no time: an event starts its runs.
        ScheduleSpec::On { .. } => Ok(None),
        // A project action has no time: a person starts each run.
        ScheduleSpec::Manual {} => Ok(None),
        // A start-of-day schedule has no time of its own: a detected
        // start plans its run ([`arm_day_start`]).
        ScheduleSpec::DayStart { .. } => Ok(None),
    }
}

// ------------------------------------------------------ the start of a day

/// The longest `delay` of a `day_start` spec, in seconds (12 hours).
pub const MAX_DAY_START_DELAY: i64 = 12 * 3600;
/// A run planned on one day for a time after midnight still runs on the
/// next day when the daemon is up at that time, within this many seconds.
/// A plan that the machine slept past is dropped.
pub const DAY_START_GRACE_SECS: i64 = 60;
/// The most `day_start` schedules that one pass reads.
pub const DAY_START_BATCH: usize = 1000;

/// Parse a local time as `HH:MM`.
pub fn parse_hhmm(s: &str) -> Result<NaiveTime, String> {
    NaiveTime::parse_from_str(s.trim(), "%H:%M")
        .map_err(|_| format!("`{s}` is not a time as HH:MM"))
}

/// Parse a day of the week: `mon` to `sun`, or the full name.
pub fn parse_weekday(s: &str) -> Result<chrono::Weekday, String> {
    use chrono::Weekday::*;
    Ok(match s.trim().to_ascii_lowercase().as_str() {
        "mon" | "monday" => Mon,
        "tue" | "tuesday" => Tue,
        "wed" | "wednesday" => Wed,
        "thu" | "thursday" => Thu,
        "fri" | "friday" => Fri,
        "sat" | "saturday" => Sat,
        "sun" | "sunday" => Sun,
        _ => {
            return Err(format!(
                "`{s}` is not a day: use mon, tue, wed, thu, fri, sat, or sun"
            ));
        }
    })
}

/// What started the day for a `day_start` schedule: the detected start
/// that plans its run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartCause {
    /// The daemon started.
    DaemonStart,
    /// The timer ran again after a gap (the machine slept).
    Resume,
    /// The host agent of the action's target came back online.
    HostReconnect,
    /// The day changed while the daemon was up.
    DayChange,
}

impl StartCause {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DaemonStart => "daemon_start",
            Self::Resume => "resume",
            Self::HostReconnect => "host_reconnect",
            Self::DayChange => "day_change",
        }
    }
}

/// Which instant of a local time that a clock change shows twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Repeat {
    /// The first: `not_before` opens the window as early as it can.
    First,
    /// The second: `not_after` closes the window as late as it can.
    Last,
}

/// The time of `t` on `day` in `zone`. A time in the gap of a clock
/// change (spring forward) moves on by one hour; a time that the clock
/// shows twice (fall back) is the first or the last instant.
fn local_at(zone: chrono_tz::Tz, day: NaiveDate, t: NaiveTime, repeat: Repeat) -> DateTime<Utc> {
    let naive = day.and_time(t);
    let pick = |r: chrono::LocalResult<DateTime<chrono_tz::Tz>>| match repeat {
        Repeat::First => r.earliest(),
        Repeat::Last => r.latest(),
    };
    pick(zone.from_local_datetime(&naive))
        .or_else(|| pick(zone.from_local_datetime(&(naive + chrono::Duration::hours(1)))))
        .map_or_else(|| naive.and_utc(), |x| x.with_timezone(&Utc))
}

/// The day of `at` in the zone of a `day_start` spec, as `YYYY-MM-DD`.
pub fn day_start_day(spec: &ScheduleSpec, at: DateTime<Utc>) -> ApiResult<Option<NaiveDate>> {
    let ScheduleSpec::DayStart { tz, .. } = spec else {
        return Ok(None);
    };
    let zone = parse_tz(tz).map_err(|e| field_error("spec.tz", e))?;
    Ok(Some(at.with_timezone(&zone).date_naive()))
}

/// The run of a `day_start` spec for a start at `now`: its time (`now`
/// plus `delay`, and not before `not_before`) and its local day. `None`:
/// no run for this start: the day ran already (`done`), the day is not in
/// `weekdays`, the run would be after `not_after`, or the spec is of
/// another kind.
pub fn day_start_plan(
    spec: &ScheduleSpec,
    done: Option<&str>,
    now: DateTime<Utc>,
) -> ApiResult<Option<(DateTime<Utc>, NaiveDate)>> {
    let ScheduleSpec::DayStart {
        tz,
        not_before,
        not_after,
        delay,
        weekdays,
    } = spec
    else {
        return Ok(None);
    };
    let zone = parse_tz(tz).map_err(|e| field_error("spec.tz", e))?;
    let local = now.with_timezone(&zone);
    let day = local.date_naive();
    if done
        .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
        .is_some_and(|d| d >= day)
    {
        return Ok(None);
    }
    if !weekdays.is_empty()
        && !weekdays
            .iter()
            .any(|w| parse_weekday(w).is_ok_and(|d| d == local.weekday()))
    {
        return Ok(None);
    }
    let secs = match delay {
        Some(d) => parse_every(d).map_err(|e| field_error("spec.delay", e))?,
        None => 0,
    };
    let mut at = now + chrono::Duration::seconds(secs);
    if let Some(nb) = not_before {
        let t = parse_hhmm(nb).map_err(|e| field_error("spec.not_before", e))?;
        at = at.max(local_at(zone, day, t, Repeat::First));
    }
    if let Some(na) = not_after {
        let t = parse_hhmm(na).map_err(|e| field_error("spec.not_after", e))?;
        if at > local_at(zone, day, t, Repeat::Last) {
            return Ok(None);
        }
    }
    Ok(Some((at, day)))
}

/// The enabled `day_start` schedules, at most [`DAY_START_BATCH`]. The
/// index `schedules_spec_kind` serves the read.
pub fn day_starts(conn: &Connection) -> ApiResult<Vec<Schedule>> {
    let limit = DAY_START_BATCH as i64;
    query(
        conn,
        "SELECT * FROM schedules WHERE json_extract(spec, '$.kind') = 'day_start'
           AND enabled = 1 ORDER BY id LIMIT ?1",
        &[&limit],
    )
}

/// The detected start of a planned run: its cause and time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedStart {
    /// `daemon_start`, `resume`, `host_reconnect`, or `day_change`.
    pub cause: String,
    pub at: String,
}

/// Plan the run of a `day_start` schedule for a start at `now`: set its
/// `next_run_at` and its detected start. Returns the run time, or `None`
/// when this start plans no run ([`day_start_plan`]).
pub fn arm_day_start(
    t: &mut Tx<'_>,
    s: &Schedule,
    cause: StartCause,
    now: DateTime<Utc>,
) -> ApiResult<Option<String>> {
    let Some((at, _)) = day_start_plan(&s.spec, s.day_start_done.as_deref(), now)? else {
        return Ok(None);
    };
    let start = DetectedStart {
        cause: cause.as_str().into(),
        at: stamp(now),
    };
    let at = stamp(at);
    t.execute(
        "UPDATE schedules SET next_run_at = ?2, day_start_last = ?3 WHERE id = ?1",
        params![s.id, at, serde_json::to_string(&start)?],
    )?;
    Ok(Some(at))
}

/// Is the planned run of `s` still good at `now`? The day of a plan is
/// the day of its detected start. A plan for an earlier day still runs at
/// its time after midnight (a late start with a delay), but a plan that
/// the machine slept past (more than [`DAY_START_GRACE_SECS`] late) on a
/// later day is dropped, and so is a plan past `not_after` of its day.
pub fn day_start_armed_valid(s: &Schedule, now: DateTime<Utc>) -> ApiResult<bool> {
    let ScheduleSpec::DayStart { tz, not_after, .. } = &s.spec else {
        return Ok(false);
    };
    let zone = parse_tz(tz).map_err(|e| field_error("spec.tz", e))?;
    let today = now.with_timezone(&zone).date_naive();
    let planned = s
        .day_start_last
        .as_ref()
        .and_then(|d| parse_time(&d.at))
        .map(|at| at.with_timezone(&zone).date_naive());
    let (Some(planned), Some(next)) = (planned, s.next_run_at.as_deref().and_then(parse_time))
    else {
        return Ok(false);
    };
    if planned < today && now > next + chrono::Duration::seconds(DAY_START_GRACE_SECS) {
        return Ok(false);
    }
    if let Some(na) = not_after {
        let t = parse_hhmm(na).map_err(|e| field_error("spec.not_after", e))?;
        if now > local_at(zone, planned, t, Repeat::Last) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Drop the planned run of a `day_start` schedule.
pub fn disarm_day_start(t: &Tx<'_>, id: &str) -> ApiResult<()> {
    t.execute(
        "UPDATE schedules SET next_run_at = NULL WHERE id = ?1",
        [id],
    )?;
    Ok(())
}

/// Record that the day of the detected start of `s` ran: no other start
/// on that day plans a run.
pub fn mark_day_started(t: &Tx<'_>, s: &Schedule, now: DateTime<Utc>) -> ApiResult<()> {
    let at = s
        .day_start_last
        .as_ref()
        .and_then(|d| parse_time(&d.at))
        .unwrap_or(now);
    if let Some(day) = day_start_day(&s.spec, at)? {
        t.execute(
            "UPDATE schedules SET day_start_done = ?2 WHERE id = ?1",
            params![s.id, day.to_string()],
        )?;
    }
    Ok(())
}

// --------------------------------------------------------------- the rows

/// The last run of a schedule, on the schedule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastRun {
    pub at: String,
    /// `ok`, `failed`, or `skipped`.
    pub status: String,
    pub summary: String,
    pub run_id: String,
}

/// A schedule as the API shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Schedule {
    pub id: String,
    pub name: String,
    /// `None`: a global schedule.
    pub project: Option<String>,
    pub enabled: bool,
    pub spec: ScheduleSpec,
    pub action: ScheduleAction,
    pub catch_up: CatchUp,
    pub max_concurrency: u32,
    /// `operator` or `agent:<ulid>`.
    pub created_by: String,
    /// `actor` (the runs act as `run_as`), `project` (the rights come from
    /// the project), or `system` (docs/api.md §19.6).
    pub owner: Owner,
    /// The actor the runs of an actor-owned schedule act as. The daemon
    /// checks its rights at each run. A project- or system-owned schedule
    /// does not use it.
    pub run_as: String,
    pub next_run_at: Option<String>,
    pub last_run: Option<LastRun>,
    pub runs_count: i64,
    /// `api`, `file`, or `fleet:<name>`.
    pub source: String,
    /// Rights checks in a row that skipped a run.
    pub consecutive_skips: i64,
    /// Why the daemon disabled the schedule, when it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    /// A webhook action: the failed attempts in a row (the 20th disables
    /// it).
    #[serde(skip_serializing_if = "is_zero")]
    pub webhook_failures: i64,
    /// A `day_start` schedule: the local day (`YYYY-MM-DD`) of its last
    /// run. A start on that day plans no run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub day_start_done: Option<String>,
    /// A `day_start` schedule: the last detected start that planned a run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub day_start_last: Option<DetectedStart>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip)]
    pub anchor_at: String,
}

impl Schedule {
    /// `project/name`, or the name of a global schedule.
    pub fn key(&self) -> String {
        match &self.project {
            Some(p) => format!("{p}/{}", self.name),
            None => self.name.clone(),
        }
    }

    fn anchor(&self) -> DateTime<Utc> {
        parse_time(&self.anchor_at).unwrap_or_else(Utc::now)
    }

    /// The actor label of a timed run's writes: the `run_as`, or
    /// `schedule:<id>` for a project- or system-owned schedule.
    pub fn run_label(&self) -> String {
        if self.owner.is_owned() {
            run_label(&self.id)
        } else {
            self.run_as.clone()
        }
    }

    /// The definition (for a fleet export and the fleet plan).
    pub fn def(&self) -> ScheduleDef {
        ScheduleDef {
            name: self.name.clone(),
            spec: self.spec.clone(),
            action: self.action.clone(),
            enabled: self.enabled,
            catch_up: self.catch_up,
        }
    }
}

fn row(r: &Row<'_>) -> rusqlite::Result<(Schedule, String, String, Option<String>, String)> {
    let s = Schedule {
        id: r.get("id")?,
        name: r.get("name")?,
        project: r.get("project")?,
        enabled: r.get("enabled")?,
        spec: ScheduleSpec::At { at: String::new() },
        action: ScheduleAction::Probe {
            subscription: String::new(),
        },
        catch_up: CatchUp::Skip,
        max_concurrency: r.get("max_concurrency")?,
        created_by: r.get("created_by")?,
        owner: Owner::from_db(&r.get::<_, String>("owner")?),
        run_as: r.get("run_as")?,
        next_run_at: r.get("next_run_at")?,
        last_run: None,
        runs_count: r.get("runs_count")?,
        source: r.get("source")?,
        consecutive_skips: r.get("skips")?,
        disabled_reason: r.get("disabled_reason")?,
        webhook_failures: r.get("webhook_failures")?,
        day_start_done: r.get("day_start_done")?,
        day_start_last: r
            .get::<_, Option<String>>("day_start_last")?
            .and_then(|d| serde_json::from_str(&d).ok()),
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        anchor_at: r.get("anchor_at")?,
    };
    Ok((
        s,
        r.get("spec")?,
        r.get("action")?,
        r.get("last_run")?,
        r.get("catch_up")?,
    ))
}

fn finish_row(
    (mut s, spec, action, last, catch_up): (Schedule, String, String, Option<String>, String),
) -> ApiResult<Schedule> {
    s.spec = serde_json::from_str(&spec)?;
    s.action = serde_json::from_str(&action)?;
    s.last_run = last.and_then(|l| serde_json::from_str(&l).ok());
    s.catch_up = if catch_up == "once" {
        CatchUp::Once
    } else {
        CatchUp::Skip
    };
    Ok(s)
}

fn query(conn: &Connection, sql: &str, args: &[&dyn rusqlite::ToSql]) -> ApiResult<Vec<Schedule>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(args, row)?.collect::<Result<Vec<_>, _>>()?;
    rows.into_iter().map(finish_row).collect()
}

pub fn get(conn: &Connection, id: &str) -> ApiResult<Schedule> {
    query(conn, "SELECT * FROM schedules WHERE id = ?1", &[&id])?
        .pop()
        .ok_or_else(|| ApiError::not_found(format!("no schedule `{id}`")))
}

/// The schedule `name` in `project` (`None`: global).
pub fn find_in(
    conn: &Connection,
    project: Option<&str>,
    name: &str,
) -> ApiResult<Option<Schedule>> {
    let scope = project.unwrap_or_default();
    Ok(query(
        conn,
        "SELECT * FROM schedules WHERE scope = ?1 AND name = ?2",
        &[&scope, &name],
    )?
    .pop())
}

/// Find a schedule by ULID, `project/name`, or a name: in `context` first,
/// then among the global schedules.
pub fn find(conn: &Connection, key: &str, context: Option<&str>) -> ApiResult<Schedule> {
    let key = key.trim();
    if key.parse::<ulid::Ulid>().is_ok()
        && let Ok(s) = get(conn, key)
    {
        return Ok(s);
    }
    let found = match key.split_once('/') {
        Some((p, n)) => find_in(conn, Some(p), n)?,
        None => match context {
            Some(p) => match find_in(conn, Some(p), key)? {
                Some(s) => Some(s),
                None => find_in(conn, None, key)?,
            },
            None => find_in(conn, None, key)?,
        },
    };
    found.ok_or_else(|| ApiError::not_found(format!("no schedule `{key}`")))
}

/// The filters of a list.
#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    /// Only this project's schedules (and the global ones with `global`).
    pub project: Option<String>,
    /// With `project`: the global schedules too. Without: only the global
    /// schedules when no `projects` list is given.
    pub global: bool,
    /// The projects a caller may read; `None` means all.
    pub projects: Option<Vec<String>>,
    pub enabled: Option<bool>,
    /// Only one group of specs. `None`: all.
    pub group: Option<SpecGroup>,
}

/// The groups of a list filter (`kind=` of `GET /api/schedules`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecGroup {
    /// `cron`, `interval`, and `at`.
    Time,
    /// `on`.
    Trigger,
    /// `manual`: the project actions (TER-53).
    Action,
}

impl SpecGroup {
    /// The group of a spec.
    pub fn of(spec: &ScheduleSpec) -> Self {
        match spec {
            ScheduleSpec::On { .. } => Self::Trigger,
            ScheduleSpec::Manual {} => Self::Action,
            _ => Self::Time,
        }
    }
}

/// Schedules, global first, then by project and name.
pub fn list(conn: &Connection, f: &ListFilter) -> ApiResult<Vec<Schedule>> {
    let all = query(conn, "SELECT * FROM schedules ORDER BY scope, name", &[])?;
    Ok(all
        .into_iter()
        .filter(|s| match (&s.project, &f.project) {
            (None, _) => f.global,
            (Some(p), Some(want)) => p == want,
            (Some(p), None) => f.projects.as_ref().is_none_or(|ps| ps.contains(p)),
        })
        .filter(|s| f.enabled.is_none_or(|e| e == s.enabled))
        .filter(|s| f.group.is_none_or(|g| g == SpecGroup::of(&s.spec)))
        .collect())
}

/// The schedules of a project, for a fleet export.
pub fn of_project(conn: &Connection, project: &str) -> ApiResult<Vec<Schedule>> {
    query(
        conn,
        "SELECT * FROM schedules WHERE project = ?1 ORDER BY name",
        &[&project],
    )
}

/// Enabled schedules with `next_run_at` at or before `now`.
pub fn due(conn: &Connection, now: DateTime<Utc>) -> ApiResult<Vec<Schedule>> {
    let now = stamp(now);
    query(
        conn,
        "SELECT * FROM schedules WHERE enabled = 1 AND next_run_at IS NOT NULL
           AND next_run_at <= ?1 ORDER BY next_run_at, id",
        &[&now],
    )
}

/// Enabled schedules whose next run is before `before`: at a daemon start,
/// the runs it missed while it was down.
pub fn missed(conn: &Connection, before: DateTime<Utc>) -> ApiResult<Vec<Schedule>> {
    let before = stamp(before);
    query(
        conn,
        "SELECT * FROM schedules WHERE enabled = 1 AND next_run_at IS NOT NULL
           AND next_run_at < ?1 ORDER BY next_run_at, id",
        &[&before],
    )
}

// ------------------------------------------------- what an action reaches

/// What an action touches, resolved in the schedule's project. The policy
/// (`policy::manage_schedule`) checks it against the actor.
#[derive(Debug, Clone, PartialEq)]
pub enum Reach {
    Mail(Address),
    Agent {
        id: String,
        name: String,
        project: String,
    },
    Target {
        name: String,
        project: Option<String>,
    },
    Subscription {
        name: String,
        /// The projects whose agents use the subscription.
        projects: Vec<String>,
    },
    /// The board of a project (a `card` action).
    Card {
        project: String,
    },
    /// An HTTP request to a URL (a `webhook` action). `host` is for the
    /// summaries: the host, or `secret <name>` for a URL from a secret.
    Webhook {
        host: String,
    },
}

impl Reach {
    /// A readable name of the object: for summaries.
    pub fn display(&self) -> String {
        match self {
            Self::Mail(a) => a.display(),
            Self::Agent { name, project, .. } => format!("{project}/{name}"),
            Self::Target { name, .. } | Self::Subscription { name, .. } => name.clone(),
            Self::Card { project } => format!("the board of {project}"),
            Self::Webhook { host } => format!("webhook {host}"),
        }
    }
}

fn not_found_as_field(e: ApiError, field: &str) -> ApiError {
    if e.code == Code::NotFound {
        field_error(field, e.message)
    } else {
        e
    }
}

/// Check the fields of an action and find what it touches. `project` is
/// the schedule's project: bare agent names resolve there.
pub fn resolve_reach(
    conn: &Connection,
    project: Option<&str>,
    action: &ScheduleAction,
) -> ApiResult<Reach> {
    let agent = |key: &str| -> ApiResult<Reach> {
        if key.trim().is_empty() {
            return Err(field_error("action.agent", "name an agent"));
        }
        let a =
            agents::find(conn, key, project).map_err(|e| not_found_as_field(e, "action.agent"))?;
        Ok(Reach::Agent {
            id: a.agent.id.to_string(),
            name: a.agent.name,
            project: a.agent.project,
        })
    };
    match action {
        ScheduleAction::Mail { to, body, .. } => {
            if body.trim().is_empty() {
                return Err(field_error("action.body", "the mail body is empty"));
            }
            if to.trim().is_empty() {
                return Err(field_error("action.to", "name an address"));
            }
            let a =
                mail::resolve(conn, to, project).map_err(|e| not_found_as_field(e, "action.to"))?;
            if a == Address::System {
                return Err(field_error("action.to", "`system` only sends mail"));
            }
            Ok(Reach::Mail(a))
        }
        ScheduleAction::Turn {
            agent: key, text, ..
        } => {
            if text.trim().is_empty() {
                return Err(field_error("action.text", "the turn text is empty"));
            }
            agent(key)
        }
        ScheduleAction::Agent { agent: key, .. } => agent(key),
        ScheduleAction::Exec {
            target,
            cmd,
            timeout_secs,
            cwd,
        } => {
            if cmd.is_empty() || cmd[0].trim().is_empty() {
                return Err(field_error("action.cmd", "the command is empty"));
            }
            if let Some(t) = timeout_secs
                && (*t == 0 || *t > MAX_EXEC_TIMEOUT)
            {
                return Err(field_error(
                    "action.timeout_secs",
                    format!("{t} is not 1-{MAX_EXEC_TIMEOUT}"),
                ));
            }
            if let Some(c) = cwd
                && !c.starts_with('/')
            {
                return Err(field_error("action.cwd", "use an absolute path"));
            }
            let row = targets::find(conn, target)?
                .ok_or_else(|| field_error("action.target", format!("no target `{target}`")))?;
            Ok(Reach::Target {
                name: row.def.name,
                project: row.def.project,
            })
        }
        ScheduleAction::Probe { subscription } => {
            let s = subscriptions::find(conn, subscription)?.ok_or_else(|| {
                field_error(
                    "action.subscription",
                    format!("no subscription `{subscription}`"),
                )
            })?;
            let mut stmt = conn.prepare(
                "SELECT DISTINCT project FROM agents WHERE subscription = ?1 ORDER BY project",
            )?;
            let projects = stmt
                .query_map([&s.name], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Reach::Subscription {
                name: s.name,
                projects,
            })
        }
        ScheduleAction::Card {
            op, key, column, ..
        } => {
            let Some(p) = project else {
                return Err(field_error(
                    "action",
                    "a card action needs a project schedule: the card goes on its board",
                ));
            };
            if key.trim().is_empty() {
                return Err(field_error("action.key", "name the card key"));
            }
            if let Some(c) = column {
                // A role alias (`working`, `blocked`) names the column with
                // that role; the action resolves it again when it runs.
                let Ok(c) = board::resolve_column(conn, p, c) else {
                    return Err(field_error(
                        "action.column",
                        format!("project `{p}` has no column `{c}`"),
                    ));
                };
                if *op == CardOp::Open && c == board::DONE {
                    return Err(field_error("action.column", "a card does not open in done"));
                }
            }
            Ok(Reach::Card {
                project: p.to_owned(),
            })
        }
        ScheduleAction::Webhook(a) => {
            a.check().map_err(|(f, m)| field_error(f, m))?;
            let host = match &a.url_secret {
                Some(name) => format!("secret {name}"),
                None => crate::webhooks::net::check_url(&a.url)
                    .map_err(|e| field_error("action.url", e))?
                    .host_str()
                    .unwrap_or_default()
                    .to_owned(),
            };
            Ok(Reach::Webhook { host })
        }
    }
}

// ------------------------------------------------------------- the writes

fn emit(t: &mut Tx<'_>, change: &str, s: &Schedule, run: Option<&Run>) -> ApiResult<()> {
    let mut data = json!({
        "change": change,
        "schedule": s.id,
        "name": s.name,
        "project": s.project,
    });
    let mut summary = format!("schedule {}: {change}", s.key());
    if let Some(r) = run {
        data["run"] = json!(r.id);
        data["status"] = json!(r.status);
        data["trigger"] = json!(r.trigger);
        if change == "run_finished" {
            summary = format!("schedule {}: {} ({})", s.key(), r.status, r.summary);
        }
    }
    let mut draft = EventDraft::new("schedule", summary, data);
    if let Some(p) = &s.project {
        draft = draft.project(p.clone());
    }
    t.emit(draft)?;
    Ok(())
}

/// A new schedule.
#[derive(Debug, Clone)]
pub struct NewSchedule {
    pub name: String,
    pub project: Option<String>,
    pub spec: ScheduleSpec,
    pub action: ScheduleAction,
    pub enabled: bool,
    pub catch_up: CatchUp,
    /// `operator` or `agent:<ulid>`; also the `run_as`.
    pub created_by: String,
    /// `api`, `file`, or `fleet:<name>`.
    pub source: String,
    pub owner: Owner,
}

/// The check that every write of a name does.
pub fn check_name(name: &str) -> ApiResult<()> {
    if valid_name(name) {
        Ok(())
    } else {
        Err(field_error(
            "name",
            format!(
                "bad schedule name `{name}`: use 1-63 of a-z, 0-9, `-`, `_`, starting with a letter or digit"
            ),
        ))
    }
}

/// Write a new schedule. The caller checked the rights.
pub fn create(t: &mut Tx<'_>, n: &NewSchedule, now: DateTime<Utc>) -> ApiResult<Schedule> {
    check_name(&n.name)?;
    validate_spec(&n.spec, now)?;
    check_trigger(t, n.project.as_deref(), &n.spec, &n.action)?;
    if let Some(p) = &n.project {
        projects::get(t, p).map_err(|e| not_found_as_field(e, "project"))?;
        let count: i64 = t.query_row(
            "SELECT count(*) FROM schedules WHERE project = ?1",
            [p],
            |r| r.get(0),
        )?;
        let max = max_per_project();
        if usize::try_from(count).unwrap_or(usize::MAX) >= max {
            return Err(ApiError::conflict(format!(
                "project `{p}` has {count} schedules; the most is {max} ({ENV_MAX_SCHEDULES})"
            )));
        }
    }
    if find_in(t, n.project.as_deref(), &n.name)?.is_some() {
        return Err(ApiError::conflict(format!(
            "schedule `{}` exists already in {}",
            n.name,
            n.project.as_deref().map_or_else(
                || "the global scope".to_owned(),
                |p| format!("project `{p}`")
            )
        )));
    }
    let id = ulid::Ulid::new().to_string();
    let at = stamp(now);
    let next = if n.enabled {
        next_after(&n.spec, now, now)?.map(stamp)
    } else {
        None
    };
    t.execute(
        "INSERT INTO schedules (id, name, project, scope, enabled, spec, action, catch_up,
                                max_concurrency, created_by, run_as, anchor_at, next_run_at,
                                source, created_at, updated_at, owner)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, ?9, ?9, ?10, ?11, ?12, ?10, ?10, ?13)",
        params![
            id,
            n.name,
            n.project,
            n.project.clone().unwrap_or_default(),
            n.enabled,
            serde_json::to_string(&n.spec)?,
            serde_json::to_string(&n.action)?,
            n.catch_up.as_str(),
            n.created_by,
            at,
            next,
            n.source,
            n.owner.as_str(),
        ],
    )?;
    let s = get(t, &id)?;
    emit(t, "created", &s, None)?;
    t.audit(
        "schedule.create",
        &s.key(),
        s.project.as_deref(),
        json!({"id": s.id, "spec": s.spec, "action": s.action.kind(), "source": s.source,
               "owner": s.owner.as_str()}),
    )?;
    Ok(s)
}

/// The fields a `PATCH` may change.
#[derive(Debug, Clone, Default)]
pub struct SchedulePatch {
    pub name: Option<String>,
    pub spec: Option<ScheduleSpec>,
    pub action: Option<ScheduleAction>,
    pub enabled: Option<bool>,
    pub catch_up: Option<CatchUp>,
    /// A new `run_as`: the caller that changed the action. It applies
    /// when the action or the owner changes, or with `take_over`.
    pub run_as: Option<String>,
    pub source: Option<String>,
    /// A new owner (docs/api.md §19.6).
    pub owner: Option<Owner>,
    /// The caller takes the schedule over: `run_as` applies with no other
    /// change.
    pub take_over: bool,
}

/// Change a schedule. The caller checked the rights. A new spec starts a
/// new interval from `now`.
pub fn update(
    t: &mut Tx<'_>,
    id: &str,
    p: &SchedulePatch,
    now: DateTime<Utc>,
) -> ApiResult<Schedule> {
    let old = get(t, id)?;
    let mut s = old.clone();
    let mut changed = Vec::new();
    if let Some(n) = p.name.as_ref().filter(|n| **n != old.name) {
        check_name(n)?;
        if find_in(t, old.project.as_deref(), n)?.is_some() {
            return Err(ApiError::conflict(format!("schedule `{n}` exists already")));
        }
        s.name = n.clone();
        changed.push("name");
    }
    let mut new_anchor = false;
    if let Some(spec) = p.spec.as_ref().filter(|x| **x != old.spec) {
        validate_spec(spec, now)?;
        s.spec = spec.clone();
        new_anchor = true;
        changed.push("spec");
    }
    if let Some(a) = p.action.as_ref().filter(|x| **x != old.action) {
        s.action = a.clone();
        changed.push("action");
    }
    if let Some(o) = p.owner.filter(|x| *x != old.owner) {
        s.owner = o;
        changed.push("owner");
    }
    let takes = changed.contains(&"action") || changed.contains(&"owner") || p.take_over;
    if let Some(r) = p.run_as.as_ref().filter(|r| takes && **r != old.run_as) {
        s.run_as = r.clone();
        changed.push("run_as");
    }
    if let Some(c) = p.catch_up.filter(|x| *x != old.catch_up) {
        s.catch_up = c;
        changed.push("catch_up");
    }
    if let Some(e) = p.enabled.filter(|x| *x != old.enabled) {
        s.enabled = e;
        changed.push("enabled");
    }
    if let Some(src) = &p.source {
        s.source = src.clone();
    }
    if changed.contains(&"spec") || changed.contains(&"action") {
        check_trigger(t, s.project.as_deref(), &s.spec, &s.action)?;
    }
    if new_anchor {
        s.anchor_at = stamp(now);
    }
    if changed.contains(&"spec") || changed.contains(&"enabled") {
        s.next_run_at = if s.enabled {
            next_after(&s.spec, s.anchor(), now)?.map(stamp)
        } else {
            None
        };
    }
    if changed.contains(&"enabled") && s.enabled {
        s.consecutive_skips = 0;
        s.disabled_reason = None;
        crate::store::webhooks::reset_failures(t, id)?;
    }
    if changed.is_empty() && s.source == old.source {
        return Ok(old);
    }
    t.execute(
        "UPDATE schedules SET name = ?2, spec = ?3, action = ?4, catch_up = ?5, enabled = ?6,
                run_as = ?7, anchor_at = ?8, next_run_at = ?9, skips = ?10,
                disabled_reason = ?11, source = ?12, updated_at = ?13, owner = ?14
         WHERE id = ?1",
        params![
            id,
            s.name,
            serde_json::to_string(&s.spec)?,
            serde_json::to_string(&s.action)?,
            s.catch_up.as_str(),
            s.enabled,
            s.run_as,
            s.anchor_at,
            s.next_run_at,
            s.consecutive_skips,
            s.disabled_reason,
            s.source,
            stamp(now),
            s.owner.as_str(),
        ],
    )?;
    let s = get(t, id)?;
    if !changed.is_empty() {
        let change = match changed.as_slice() {
            ["enabled"] if s.enabled => "enabled",
            ["enabled"] => "disabled",
            _ => "updated",
        };
        emit(t, change, &s, None)?;
        t.audit(
            "schedule.update",
            &s.key(),
            s.project.as_deref(),
            json!({"id": s.id, "changed": changed, "enabled": s.enabled,
                   "owner": s.owner.as_str(), "run_as": s.run_as}),
        )?;
    }
    Ok(s)
}

/// Enable or disable a schedule. `reason` is kept on a disable.
pub fn set_enabled(
    t: &mut Tx<'_>,
    id: &str,
    enabled: bool,
    reason: Option<&str>,
    now: DateTime<Utc>,
) -> ApiResult<Schedule> {
    let s = update(
        t,
        id,
        &SchedulePatch {
            enabled: Some(enabled),
            ..Default::default()
        },
        now,
    )?;
    if !enabled && let Some(r) = reason {
        t.execute(
            "UPDATE schedules SET disabled_reason = ?2 WHERE id = ?1",
            params![id, r],
        )?;
        return get(t, id);
    }
    Ok(s)
}

pub fn delete(t: &mut Tx<'_>, id: &str) -> ApiResult<Schedule> {
    let s = get(t, id)?;
    t.execute("DELETE FROM schedules WHERE id = ?1", [id])?;
    emit(t, "deleted", &s, None)?;
    t.audit(
        "schedule.delete",
        &s.key(),
        s.project.as_deref(),
        json!({"id": s.id, "source": s.source}),
    )?;
    Ok(s)
}

/// Move `next_run_at` to the first run after `now`.
pub fn advance(t: &mut Tx<'_>, id: &str, now: DateTime<Utc>) -> ApiResult<Option<String>> {
    let s = get(t, id)?;
    let next = next_after(&s.spec, s.anchor(), now)?.map(stamp);
    t.execute(
        "UPDATE schedules SET next_run_at = ?2 WHERE id = ?1",
        params![id, next],
    )?;
    Ok(next)
}

/// Write a definition (a project file or a fleet) into `project`: a new
/// schedule, or the changed fields of the one with the same name. A new
/// schedule runs as `created_by`; a changed action runs as it too.
pub fn upsert_def(
    t: &mut Tx<'_>,
    project: &str,
    def: &ScheduleDef,
    source: &str,
    created_by: &str,
    now: DateTime<Utc>,
) -> ApiResult<Schedule> {
    match find_in(t, Some(project), &def.name)? {
        None => create(
            t,
            &NewSchedule {
                name: def.name.clone(),
                project: Some(project.to_owned()),
                spec: def.spec.clone(),
                action: def.action.clone(),
                enabled: def.enabled,
                catch_up: def.catch_up,
                created_by: created_by.to_owned(),
                source: source.to_owned(),
                owner: Owner::Actor,
            },
            now,
        ),
        Some(s) => update(
            t,
            &s.id,
            &SchedulePatch {
                name: None,
                spec: Some(def.spec.clone()),
                action: Some(def.action.clone()),
                enabled: Some(def.enabled),
                catch_up: Some(def.catch_up),
                run_as: Some(created_by.to_owned()),
                source: Some(source.to_owned()),
                ..Default::default()
            },
            now,
        ),
    }
}

/// Sync the `[[schedules]]` of a project file: write each one with
/// `source = file`, and delete the file schedules of the project that the
/// file no longer has. Returns the errors, one line each.
pub fn sync_file(
    t: &mut Tx<'_>,
    project: &str,
    defs: &[ScheduleDef],
    now: DateTime<Utc>,
) -> ApiResult<Vec<String>> {
    let mut errors = Vec::new();
    for d in defs {
        if let Err(e) = resolve_reach(t, Some(project), &d.action)
            .and_then(|_| upsert_def(t, project, d, projects::SOURCE_FILE, "operator", now))
        {
            errors.push(format!(
                "projects/{project}.toml schedule {}: {}",
                d.name, e.message
            ));
        }
    }
    for s in of_project(t, project)? {
        if s.source == projects::SOURCE_FILE && !defs.iter().any(|d| d.name == s.name) {
            delete(t, &s.id)?;
        }
    }
    Ok(errors)
}

/// A project file is gone: its file schedules become API schedules.
pub fn detach_file(t: &Tx<'_>, project: &str) -> ApiResult<()> {
    t.execute(
        "UPDATE schedules SET source = 'api' WHERE project = ?1 AND source = 'file'",
        [project],
    )?;
    Ok(())
}

// -------------------------------------------------------------- the runs

/// One run of a schedule.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Run {
    pub id: String,
    pub schedule: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    /// `running`, `ok`, `failed`, or `skipped`.
    pub status: String,
    /// `cron`, `manual`, `catch_up`, or `event`.
    pub trigger: String,
    pub summary: String,
    /// Exec: the tail of stdout and stderr and the exit code. Mail: the
    /// message ID. Turn: the delivery. Agent: the new status. Probe: the
    /// windows.
    pub output: Value,
    pub error: Option<String>,
    /// Who triggered the run: the `run_as`, the caller of a run-now, or
    /// the actor of the event of a trigger run.
    pub actor: String,
    /// The `seq` of the event that started a trigger run.
    pub event_seq: Option<i64>,
    /// That event's `kind` and `summary`, while the log has it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<RunEvent>,
    /// A `day_start` run: the detected start that planned it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub day_start: Option<DetectedStart>,
}

/// The event of a trigger run, on the run.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunEvent {
    pub seq: i64,
    pub kind: String,
    pub summary: String,
}

/// The run columns, with the kind and summary of the event of a trigger
/// run.
const RUN_SELECT: &str = "SELECT r.*, e.kind AS event_kind, e.summary AS event_summary
     FROM schedule_runs r LEFT JOIN events e ON e.seq = r.event_seq";

fn run_row(r: &Row<'_>) -> rusqlite::Result<Run> {
    let output: String = r.get("output")?;
    let event_seq: Option<i64> = r.get("event_seq")?;
    let event_kind: Option<String> = r.get("event_kind")?;
    let event = match (event_seq, event_kind) {
        (Some(seq), Some(kind)) => Some(RunEvent {
            seq,
            kind,
            summary: r
                .get::<_, Option<String>>("event_summary")?
                .unwrap_or_default(),
        }),
        _ => None,
    };
    Ok(Run {
        id: r.get("id")?,
        schedule: r.get("schedule")?,
        started_at: r.get("started_at")?,
        finished_at: r.get("finished_at")?,
        status: r.get("status")?,
        trigger: r.get("trigger")?,
        summary: r.get("summary")?,
        output: serde_json::from_str(&output).unwrap_or(Value::Null),
        error: r.get("error")?,
        actor: r.get("actor")?,
        event_seq,
        event,
        day_start: r
            .get::<_, Option<String>>("day_start")?
            .and_then(|d| serde_json::from_str(&d).ok()),
    })
}

pub fn get_run(conn: &Connection, id: &str) -> ApiResult<Run> {
    conn.query_row(&format!("{RUN_SELECT} WHERE r.id = ?1"), [id], run_row)
        .optional()?
        .ok_or_else(|| ApiError::not_found(format!("no schedule run `{id}`")))
}

/// The runs of a schedule, newest first. The cursor is a run ID.
pub fn list_runs(conn: &Connection, schedule: &str, page: &Page) -> ApiResult<Paged<Run>> {
    let limit = page.limit();
    let mut stmt = conn.prepare(&format!(
        "{RUN_SELECT} WHERE r.schedule = ?1 AND (?2 = '' OR r.id < ?2)
         ORDER BY r.id DESC LIMIT ?3"
    ))?;
    let rows = stmt
        .query_map(
            params![schedule, page.cursor().unwrap_or(""), (limit + 1) as i64],
            run_row,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Paged::from_rows(rows, limit, |r: &Run| r.id.clone()))
}

/// The start of a run.
#[derive(Debug)]
pub enum Begin {
    /// The run is `running`; the caller does the action and calls
    /// [`finish_run`].
    Started(Run, Box<Schedule>),
    /// Another run of the schedule is still running: this one is
    /// `skipped` already.
    Overlap(Run),
}

fn set_last_run(t: &Tx<'_>, s: &Schedule, r: &Run, skips: i64) -> ApiResult<()> {
    let last = LastRun {
        at: r
            .finished_at
            .clone()
            .unwrap_or_else(|| r.started_at.clone()),
        status: r.status.clone(),
        summary: r.summary.clone(),
        run_id: r.id.clone(),
    };
    t.execute(
        "UPDATE schedules SET last_run = ?2, runs_count = runs_count + 1, skips = ?3
         WHERE id = ?1",
        params![s.id, serde_json::to_string(&last)?, skips],
    )?;
    Ok(())
}

/// Record a run that is `skipped` at once, with no action. It does not
/// count as a refused run.
#[allow(clippy::too_many_arguments)]
fn insert_skipped(
    t: &mut Tx<'_>,
    s: &Schedule,
    trigger: &str,
    actor: &str,
    event_seq: Option<i64>,
    reason: &str,
    error: Option<&str>,
    now: DateTime<Utc>,
) -> ApiResult<Run> {
    let run_id = ulid::Ulid::new().to_string();
    t.execute(
        "INSERT INTO schedule_runs (id, schedule, trigger, status, started_at, finished_at,
                                    summary, error, actor, event_seq)
         VALUES (?1, ?2, ?3, 'skipped', ?4, ?4, ?5, ?6, ?7, ?8)",
        params![
            run_id,
            s.id,
            trigger,
            stamp(now),
            format!("skipped: {reason}"),
            error,
            actor,
            event_seq
        ],
    )?;
    let r = get_run(t, &run_id)?;
    set_last_run(t, s, &r, s.consecutive_skips)?;
    let s = get(t, &s.id)?;
    emit(t, "run_finished", &s, Some(&r))?;
    Ok(r)
}

/// Start a run of `id`: `running`, or `skipped` when a run of the schedule
/// is still running (`max_concurrency` is 1). One transaction, so two
/// triggers cannot both start. `event_seq` is the event of a trigger run.
pub fn begin_run(
    t: &mut Tx<'_>,
    id: &str,
    trigger: &str,
    actor: &str,
    event_seq: Option<i64>,
    now: DateTime<Utc>,
) -> ApiResult<Begin> {
    let s = get(t, id)?;
    let at = stamp(now);
    let run_id = ulid::Ulid::new().to_string();
    let running: i64 = t.query_row(
        "SELECT count(*) FROM schedule_runs WHERE schedule = ?1 AND status = 'running'",
        [id],
        |r| r.get(0),
    )?;
    if running > 0 {
        let r = insert_skipped(t, &s, trigger, actor, event_seq, "still running", None, now)?;
        return Ok(Begin::Overlap(r));
    }
    // A start-of-day run keeps the detected start that planned it.
    let day_start = match (&s.day_start_last, trigger) {
        (Some(d), "day_start") => Some(serde_json::to_string(d)?),
        _ => None,
    };
    t.execute(
        "INSERT INTO schedule_runs (id, schedule, trigger, status, started_at, actor, event_seq,
                                    day_start)
         VALUES (?1, ?2, ?3, 'running', ?4, ?5, ?6, ?7)",
        params![run_id, id, trigger, at, actor, event_seq, day_start],
    )?;
    let r = get_run(t, &run_id)?;
    emit(t, "run_started", &s, Some(&r))?;
    Ok(Begin::Started(r, Box::new(s)))
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    /// `ok`, `failed`, or `skipped`.
    pub status: &'static str,
    pub summary: String,
    pub output: Value,
    pub error: Option<String>,
    /// The rights check skipped the run: it counts toward [`MAX_SKIPS`].
    pub rights: bool,
}

impl Outcome {
    pub fn ok(summary: impl Into<String>, output: Value) -> Self {
        Self {
            status: "ok",
            summary: summary.into(),
            output,
            error: None,
            rights: false,
        }
    }

    pub fn failed(summary: impl Into<String>, error: impl Into<String>, output: Value) -> Self {
        Self {
            status: "failed",
            summary: summary.into(),
            output,
            error: Some(error.into()),
            rights: false,
        }
    }

    pub fn skipped(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            status: "skipped",
            summary: format!("skipped: {reason}"),
            output: json!({}),
            error: Some(reason),
            rights: false,
        }
    }

    /// A skip of the rights check.
    pub fn refused(reason: impl Into<String>) -> Self {
        Self {
            rights: true,
            ..Self::skipped(reason)
        }
    }
}

/// End a run. A rights skip counts; the [`MAX_SKIPS`]th in a row disables
/// the schedule and mails the operator. Any other end resets the count.
pub fn finish_run(t: &mut Tx<'_>, run_id: &str, o: &Outcome, now: DateTime<Utc>) -> ApiResult<Run> {
    let at = stamp(now);
    t.execute(
        "UPDATE schedule_runs SET status = ?2, finished_at = ?3, summary = ?4, output = ?5,
                                  error = ?6
         WHERE id = ?1",
        params![
            run_id,
            o.status,
            at,
            t.redact(&o.summary),
            t.redact_string(o.output.to_string()),
            t.redact_opt(o.error.as_deref())
        ],
    )?;
    let r = get_run(t, run_id)?;
    // The schedule can be gone (deleted during the run): keep the run.
    let Ok(s) = get(t, &r.schedule) else {
        return Ok(r);
    };
    let skips = if o.rights { s.consecutive_skips + 1 } else { 0 };
    set_last_run(t, &s, &r, skips)?;
    t.audit(
        "schedule.run",
        &s.key(),
        s.project.as_deref(),
        json!({"id": s.id, "run": r.id, "trigger": r.trigger, "status": r.status,
               "summary": r.summary, "run_as": s.run_as}),
    )?;
    let s = get(t, &s.id)?;
    emit(t, "run_finished", &s, Some(&r))?;
    if o.rights && skips >= MAX_SKIPS && s.enabled {
        let why = format!(
            "{skips} runs in a row were skipped: {}",
            o.error.as_deref().unwrap_or("the rights check failed")
        );
        let s = set_enabled(t, &s.id, false, Some(&why), now)?;
        mail::send(
            t,
            Outgoing {
                from: &Address::System,
                to: &Address::Operator,
                subject: &format!("Schedule {} disabled", s.key()),
                body: &format!(
                    "The daemon disabled schedule {} ({}). {why}. It runs as {}. Fix the \
                     cause, then run `terrarium schedule enable {}`.",
                    s.key(),
                    s.id,
                    s.run_as,
                    s.id
                ),
                thread: None,
                in_reply_to: None,
            },
        )?;
    }
    Ok(r)
}

/// Runs still `running` at a daemon start ended with the old daemon.
pub fn fail_stale_runs(t: &mut Tx<'_>, now: DateTime<Utc>) -> ApiResult<usize> {
    let n = t.execute(
        "UPDATE schedule_runs SET status = 'failed', finished_at = ?1,
                error = 'the daemon stopped during the run',
                summary = 'failed: the daemon stopped during the run'
         WHERE status = 'running'",
        [stamp(now)],
    )?;
    Ok(n)
}

/// Keep the newest `keep` runs of each schedule. Returns the rows deleted.
pub fn prune_runs(t: &mut Tx<'_>, keep: usize) -> ApiResult<usize> {
    let n = t.execute(
        "DELETE FROM schedule_runs WHERE id IN (
            SELECT id FROM (
                SELECT id, row_number() OVER (PARTITION BY schedule ORDER BY id DESC) AS n
                FROM schedule_runs)
            WHERE n > ?1)",
        [keep as i64],
    )?;
    Ok(n)
}

// ------------------------------------------------------ the trigger engine

/// The deepest event chain: an event that trigger runs made through this
/// many links starts no run (PLAN §12c, "Guards").
pub const MAX_CHAIN_DEPTH: u64 = 4;
/// `trigger_state` rows that did not change for this long go at the
/// prune, except an open escalation.
pub const TRIGGER_STATE_DAYS: i64 = 8;

/// The actor of the writes of a trigger run: `schedule:<id>`. A trigger
/// never matches an event with its own label.
pub fn run_label(schedule: &str) -> String {
    format!("schedule:{schedule}")
}

/// The last event `seq` that the trigger engine read; `None` before its
/// first start.
pub fn cursor(conn: &Connection) -> ApiResult<Option<i64>> {
    Ok(conn
        .query_row("SELECT seq FROM trigger_cursor WHERE id = 1", [], |r| {
            r.get(0)
        })
        .optional()?)
}

pub fn set_cursor(t: &Tx<'_>, seq: i64) -> ApiResult<()> {
    t.execute(
        "INSERT INTO trigger_cursor (id, seq) VALUES (1, ?1)
         ON CONFLICT (id) DO UPDATE SET seq = excluded.seq",
        [seq],
    )?;
    Ok(())
}

/// The enabled triggers.
pub fn triggers(conn: &Connection) -> ApiResult<Vec<Schedule>> {
    Ok(query(
        conn,
        "SELECT * FROM schedules WHERE enabled = 1 ORDER BY scope, name",
        &[],
    )?
    .into_iter()
    .filter(|s| s.spec.is_trigger())
    .collect())
}

/// The fields of a stored event that templates and rules read.
pub fn event_view(e: &crate::events::StoredEvent) -> template::EventView<'_> {
    template::EventView {
        seq: e.seq,
        kind: &e.kind,
        summary: &e.summary,
        project: e.project.as_deref(),
        actor: e.actor.as_deref(),
        data: &e.data,
    }
}

/// The chain depth of an event: `data.cause.depth` of an event that a
/// trigger run (actor `schedule:<id>`) or a board rule run (actor
/// `rule:<project>/<rule>`, `crate::rules_engine`) made, else 0. Only
/// those actors count, so a webhook cannot claim a depth. The two engines
/// share the depth, so a chain through triggers and rules stops too.
pub fn depth_of(e: &crate::events::StoredEvent) -> u64 {
    if !e
        .actor
        .as_deref()
        .is_some_and(|a| a.starts_with("schedule:") || a.starts_with("rule:"))
    {
        return 0;
    }
    e.data["cause"]["depth"].as_u64().unwrap_or(0)
}

/// What a trigger does with one event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Run the action.
    Run,
    /// The escalation threshold: mail the escalation address once, with
    /// the count of matches in a row.
    Escalate(i64),
    /// Over `max_per_hour`: record a `skipped: rate limit` run.
    RateLimited,
}

struct StateRow {
    last_fired: Option<String>,
    failures: i64,
    escalated: bool,
}

fn state_row(conn: &Connection, schedule: &str, key: &str) -> ApiResult<Option<StateRow>> {
    Ok(conn
        .query_row(
            "SELECT last_fired, failures, escalated FROM trigger_state
             WHERE schedule = ?1 AND key = ?2",
            params![schedule, key],
            |r| {
                Ok(StateRow {
                    last_fired: r.get(0)?,
                    failures: r.get(1)?,
                    escalated: r.get(2)?,
                })
            },
        )
        .optional()?)
}

fn put_state(
    t: &Tx<'_>,
    schedule: &str,
    key: &str,
    row: &StateRow,
    now: DateTime<Utc>,
) -> ApiResult<()> {
    t.execute(
        "INSERT INTO trigger_state (schedule, key, last_fired, failures, escalated, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (schedule, key) DO UPDATE SET last_fired = excluded.last_fired,
             failures = excluded.failures, escalated = excluded.escalated,
             updated_at = excluded.updated_at",
        params![
            schedule,
            key,
            row.last_fired,
            row.failures,
            row.escalated,
            stamp(now)
        ],
    )?;
    Ok(())
}

/// Decide what trigger `s` does with event `e`, and keep its counters:
/// `None` when the event does not start a run. The checks, in order: the
/// scope (a project trigger sees only its project's events), the loop
/// guard (not an event of its own runs, not an event about itself), the
/// time (not an event from before the trigger), the escalation reset,
/// the kinds and `match`, the chain depth, the escalation count, the
/// dedupe window, and the rate limit.
pub fn decide(
    t: &Tx<'_>,
    s: &Schedule,
    e: &crate::events::StoredEvent,
    now: DateTime<Utc>,
) -> ApiResult<Option<Decision>> {
    let ScheduleSpec::On {
        dedupe,
        escalate,
        max_per_hour,
        ..
    } = &s.spec
    else {
        return Ok(None);
    };
    if let Some(p) = &s.project
        && e.project.as_deref() != Some(p.as_str())
    {
        return Ok(None);
    }
    if e.actor.as_deref() == Some(run_label(&s.id).as_str())
        || (e.kind == "schedule" && e.data["schedule"].as_str() == Some(s.id.as_str()))
    {
        return Ok(None);
    }
    if e.at < s.created_at {
        return Ok(None);
    }
    let view = event_view(e);
    if let Some(esc) = escalate
        && esc
            .reset_on
            .iter()
            .any(|g| terrarium_core::schedule::glob(g, &e.kind))
    {
        let key = format!(
            "escalate:{}",
            template::render(&esc.key, Some(&view), template::Quote::Plain)
        );
        t.execute(
            "UPDATE trigger_state SET failures = 0, escalated = 0, updated_at = ?3
             WHERE schedule = ?1 AND key = ?2",
            params![s.id, key, stamp(now)],
        )?;
    }
    if !s.spec.matches_event(&view) {
        return Ok(None);
    }
    let depth = depth_of(e);
    if depth >= MAX_CHAIN_DEPTH {
        tracing::warn!(schedule = %s.key(), event = e.seq, depth,
                       "a trigger chain is too deep: the event starts no run");
        return Ok(None);
    }
    if let Some(esc) = escalate {
        let key = format!(
            "escalate:{}",
            template::render(&esc.key, Some(&view), template::Quote::Plain)
        );
        let mut row = state_row(t, &s.id, &key)?.unwrap_or(StateRow {
            last_fired: None,
            failures: 0,
            escalated: false,
        });
        row.failures += 1;
        if row.failures >= i64::from(esc.after) {
            let first = !row.escalated;
            row.escalated = true;
            if first {
                row.last_fired = Some(stamp(now));
            }
            put_state(t, &s.id, &key, &row, now)?;
            // One mail for each streak; the action does not run.
            return Ok(first.then_some(Decision::Escalate(row.failures)));
        }
        put_state(t, &s.id, &key, &row, now)?;
    }
    let mut dedupe_row = None;
    if let Some(d) = dedupe {
        let key = format!(
            "dedupe:{}",
            template::render(&d.key, Some(&view), template::Quote::Plain)
        );
        let window = chrono::Duration::seconds(parse_every(&d.window).unwrap_or(0));
        let last = state_row(t, &s.id, &key)?
            .and_then(|r| r.last_fired)
            .and_then(|l| parse_time(&l));
        if last.is_some_and(|l| now - l < window) {
            return Ok(None);
        }
        dedupe_row = Some(key);
    }
    let hour = chrono::Duration::hours(1);
    let mut rate = state_row(t, &s.id, "rate")?.unwrap_or(StateRow {
        last_fired: None,
        failures: 0,
        escalated: false,
    });
    let start = rate.last_fired.as_deref().and_then(parse_time);
    if start.is_none_or(|st| now - st >= hour) {
        rate.last_fired = Some(stamp(now));
        rate.failures = 0;
    }
    if rate.failures >= i64::from(*max_per_hour) {
        return Ok(Some(Decision::RateLimited));
    }
    rate.failures += 1;
    put_state(t, &s.id, "rate", &rate, now)?;
    if let Some(key) = dedupe_row {
        put_state(
            t,
            &s.id,
            &key,
            &StateRow {
                last_fired: Some(stamp(now)),
                failures: 0,
                escalated: false,
            },
            now,
        )?;
    }
    Ok(Some(Decision::Run))
}

/// The `data.cause` of the events of a trigger run: the trigger, the
/// event, and the chain depth (the event's depth + 1).
pub fn cause(schedule: &str, e: &crate::events::StoredEvent) -> Value {
    json!({"schedule": schedule, "event": e.seq, "depth": depth_of(e) + 1})
}

/// Record a `skipped` trigger run for event `e` (the rate limit), in the
/// engine's transaction. Its events carry the trigger's label and cause,
/// so the loop guard sees them.
pub fn record_trigger_skip(
    t: &mut Tx<'_>,
    s: &Schedule,
    e: &crate::events::StoredEvent,
    reason: &str,
    now: DateTime<Utc>,
) -> ApiResult<Run> {
    let (actor, cause_was) = (t.actor.clone(), t.cause.clone());
    t.actor = run_label(&s.id);
    t.cause = Some(cause(&s.id, e));
    let by = e.actor.clone().unwrap_or_else(|| "system".into());
    // Not through `begin_run`: a run that is still running must not
    // change the reason.
    let result = insert_skipped(t, s, "event", &by, Some(e.seq), reason, Some(reason), now);
    t.actor = actor;
    t.cause = cause_was;
    result
}

/// Drop the `trigger_state` rows that did not change for
/// [`TRIGGER_STATE_DAYS`], except an open escalation.
pub fn prune_trigger_state(t: &Tx<'_>, now: DateTime<Utc>) -> ApiResult<usize> {
    let before = stamp(now - chrono::Duration::days(TRIGGER_STATE_DAYS));
    Ok(t.execute(
        "DELETE FROM trigger_state WHERE updated_at < ?1 AND escalated = 0",
        [before],
    )?)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(s: &str) -> DateTime<Utc> {
        parse_time(s).unwrap()
    }

    fn cron(expr: &str, tz: &str) -> ScheduleSpec {
        ScheduleSpec::Cron {
            expr: expr.into(),
            tz: tz.into(),
        }
    }

    #[test]
    fn cron_next_runs_in_the_zone() {
        let now = utc("2026-10-26T08:07:00Z");
        let next = |e: &str, tz: &str| {
            next_after(&cron(e, tz), now, now)
                .unwrap()
                .map(stamp)
                .unwrap()
        };
        assert_eq!(next("*/15 * * * *", "UTC"), "2026-10-26T08:15:00.000Z");
        // 09:00 in London on 26 Oct 2026 is GMT (UTC+0) after the change on
        // 25 Oct; in New York it is still EDT (UTC-4).
        assert_eq!(
            next("0 9 * * *", "Europe/London"),
            "2026-10-26T09:00:00.000Z"
        );
        assert_eq!(
            next("0 9 * * *", "America/New_York"),
            "2026-10-26T13:00:00.000Z"
        );
        // 2026-10-26 is a Monday. Day 0 and 7 are Sunday; 1-5 is Mon-Fri.
        assert_eq!(next("0 9 * * 0", "UTC"), "2026-11-01T09:00:00.000Z");
        assert_eq!(next("0 9 * * 7", "UTC"), "2026-11-01T09:00:00.000Z");
        assert_eq!(next("0 7 * * 1-5", "UTC"), "2026-10-27T07:00:00.000Z");
        assert_eq!(next("0 9 * * MON-FRI", "UTC"), "2026-10-26T09:00:00.000Z");
        assert_eq!(next("0 9 * * 6-7", "UTC"), "2026-10-31T09:00:00.000Z");
        assert_eq!(next("@hourly", "UTC"), "2026-10-26T09:00:00.000Z");
    }

    #[test]
    fn bad_specs_name_the_field_and_the_expression() {
        let now = utc("2026-10-26T08:07:00Z");
        let e = validate_spec(&cron("*/15 * * *", "UTC"), now).unwrap_err();
        assert_eq!(e.code, Code::Invalid);
        assert!(e.message.starts_with("spec.expr:"), "{}", e.message);
        assert!(e.message.contains("`*/15 * * *`"), "{}", e.message);
        assert_eq!(e.details.unwrap()["field"], "spec.expr");
        // Seconds are not a field: the shortest cron step is one minute.
        assert!(validate_spec(&cron("*/5 * * * * *", "UTC"), now).is_err());
        let e = validate_spec(&cron("* * * * *", "Mars/Olympus"), now).unwrap_err();
        assert!(e.message.starts_with("spec.tz:"), "{}", e.message);
        let every = |s: &str| ScheduleSpec::Interval { every: s.into() };
        let e = validate_spec(&every("30s"), now).unwrap_err();
        assert!(e.message.contains("shortest interval"), "{}", e.message);
        assert!(validate_spec(&every("60s"), now).is_ok());
        assert!(validate_spec(&every("1h30m"), now).is_ok());
        assert!(validate_spec(&every("10"), now).is_err());
        let at = |s: &str| ScheduleSpec::At { at: s.into() };
        assert!(validate_spec(&at("2026-10-26T08:00:00Z"), now).is_err());
        assert!(validate_spec(&at("tomorrow"), now).is_err());
        assert!(validate_spec(&at("2026-10-27T08:00:00+01:00"), now).is_ok());
    }

    #[test]
    fn intervals_keep_their_anchor_and_at_runs_once() {
        let anchor = utc("2026-10-26T08:00:00Z");
        let spec = ScheduleSpec::Interval {
            every: "30m".into(),
        };
        let n = |after: &str| stamp(next_after(&spec, anchor, utc(after)).unwrap().unwrap());
        assert_eq!(n("2026-10-26T08:00:00Z"), "2026-10-26T08:30:00.000Z");
        assert_eq!(n("2026-10-26T08:29:59Z"), "2026-10-26T08:30:00.000Z");
        assert_eq!(n("2026-10-26T08:30:00Z"), "2026-10-26T09:00:00.000Z");
        assert_eq!(n("2026-10-26T12:10:00Z"), "2026-10-26T12:30:00.000Z");
        let at = ScheduleSpec::At {
            at: "2026-10-26T09:00:00Z".into(),
        };
        assert!(next_after(&at, anchor, anchor).unwrap().is_some());
        assert!(
            next_after(&at, anchor, utc("2026-10-26T09:00:00Z"))
                .unwrap()
                .is_none()
        );
        // A zone with a fixed offset stays valid.
        let _ = chrono_tz::UTC.with_ymd_and_hms(2026, 1, 1, 0, 0, 0);
    }

    fn day_start(tz: &str, nb: Option<&str>, na: Option<&str>, weekdays: &[&str]) -> ScheduleSpec {
        ScheduleSpec::DayStart {
            tz: tz.into(),
            not_before: nb.map(Into::into),
            not_after: na.map(Into::into),
            delay: None,
            weekdays: weekdays.iter().map(|d| (*d).into()).collect(),
        }
    }

    /// The run time and the day of a start at `now`, as text.
    fn plan(spec: &ScheduleSpec, done: Option<&str>, now: &str) -> Option<(String, String)> {
        day_start_plan(spec, done, utc(now))
            .unwrap()
            .map(|(at, day)| (stamp(at), day.to_string()))
    }

    #[test]
    fn day_start_windows_across_clock_changes() {
        // Sydney springs forward on 2026-10-04 at 02:00 (to 03:00, +11).
        // A `not_before` in the gap moves on by one hour: 03:30 AEDT.
        let s = day_start("Australia/Sydney", Some("02:30"), None, &[]);
        assert_eq!(
            plan(&s, None, "2026-10-03T14:30:00Z"),
            Some(("2026-10-03T16:30:00.000Z".into(), "2026-10-04".into()))
        );
        // Sydney falls back on 2027-04-04 at 03:00 (to 02:00, +10): 02:30
        // comes twice. `not_before` takes the first (AEDT), `not_after`
        // the last (AEST).
        let s = day_start("Australia/Sydney", Some("02:30"), None, &[]);
        assert_eq!(
            plan(&s, None, "2027-04-03T14:00:00Z"),
            Some(("2027-04-03T15:30:00.000Z".into(), "2027-04-04".into()))
        );
        let s = day_start("Australia/Sydney", None, Some("02:30"), &[]);
        assert!(plan(&s, None, "2027-04-03T16:20:00Z").is_some());
        assert!(plan(&s, None, "2027-04-03T16:40:00Z").is_none());
        // New York springs forward on 2027-03-14 at 02:00 (to 03:00,
        // -4): a `not_after` of 02:30 is 03:30 EDT.
        let s = day_start("America/New_York", None, Some("02:30"), &[]);
        assert!(plan(&s, None, "2027-03-14T07:20:00Z").is_some());
        assert!(plan(&s, None, "2027-03-14T07:40:00Z").is_none());
        // New York falls back on 2027-11-07 at 02:00 (to 01:00, -5). A
        // window 01:00-01:30 opens at 01:00 EDT and closes at 01:30 EST.
        let s = day_start("America/New_York", Some("01:00"), Some("01:30"), &[]);
        assert_eq!(
            plan(&s, None, "2027-11-07T04:00:00Z"),
            Some(("2027-11-07T05:00:00.000Z".into(), "2027-11-07".into()))
        );
        assert_eq!(
            plan(&s, None, "2027-11-07T06:15:00Z"),
            Some(("2027-11-07T06:15:00.000Z".into(), "2027-11-07".into()))
        );
        assert!(plan(&s, None, "2027-11-07T06:40:00Z").is_none());
    }

    #[test]
    fn day_start_days_and_the_done_day_are_in_the_zone() {
        // 2026-11-09 is a Monday. In Sydney (+11) the day turns at 13:00
        // UTC; in New York (-5) at 05:00 UTC.
        let syd = day_start("Australia/Sydney", None, None, &[]);
        assert_eq!(plan(&syd, None, "2026-11-09T12:59:00Z").unwrap().1, "2026-11-09");
        assert_eq!(plan(&syd, None, "2026-11-09T13:01:00Z").unwrap().1, "2026-11-10");
        let ny = day_start("America/New_York", None, None, &[]);
        assert_eq!(plan(&ny, None, "2026-11-10T04:59:00Z").unwrap().1, "2026-11-09");
        assert_eq!(plan(&ny, None, "2026-11-10T05:01:00Z").unwrap().1, "2026-11-10");
        // The done day blocks its day and every day before it.
        assert!(plan(&syd, Some("2026-11-09"), "2026-11-09T12:59:00Z").is_none());
        assert!(plan(&syd, Some("2026-11-10"), "2026-11-09T12:59:00Z").is_none());
        assert!(plan(&syd, Some("2026-11-09"), "2026-11-09T13:01:00Z").is_some());
        assert!(plan(&ny, Some("2026-11-09"), "2026-11-10T04:59:00Z").is_none());
        assert!(plan(&ny, Some("2026-11-09"), "2026-11-10T05:01:00Z").is_some());
        // The weekdays are those of the zone: Tuesday starts in Sydney
        // while it is Monday in UTC.
        let tue = day_start("Australia/Sydney", None, None, &["tue"]);
        assert!(plan(&tue, None, "2026-11-09T12:59:00Z").is_none());
        assert!(plan(&tue, None, "2026-11-09T13:01:00Z").is_some());
    }
}
