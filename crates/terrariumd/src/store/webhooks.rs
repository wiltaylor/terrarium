//! Webhook deliveries (docs/api.md §19.5): the outbox and the log of the
//! `webhook` action.
//!
//! A run of a webhook trigger renders the body and writes one `pending`
//! row ([`enqueue`]). The delivery worker (`crate::webhooks`) sends the
//! due rows and records each attempt ([`record_attempt`]): `ok`, a retry
//! at `next_attempt_at` ([`BACKOFF`], or a longer `Retry-After`),
//! `failed` (no retry), or `giving_up`. A daemon restart loses nothing:
//! the `pending` rows stay due.
//!
//! The row keeps the rendered body only. The URL, the headers, and the
//! signature come from the trigger's action at each attempt, so the
//! database never holds a secret value. The `url` column is for the log:
//! scheme, host, port, and path, with no query (a token in a query stays
//! out of the log).
//!
//! The failed attempts in a row of a trigger count in
//! `schedules.webhook_failures`; the [`MAX_FAILURES`]th disables the
//! trigger and mails the operator once. An `ok` sets the count to zero.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use serde_json::{Value, json};

use crate::db::Tx;
use crate::error::{ApiError, ApiResult};
use crate::events::EventDraft;
use crate::store::mail::{self, Address, Outgoing};
use crate::store::schedules::{self, Schedule, stamp};
use crate::store::{Page, Paged};

/// The waits before each retry, in seconds: 10 s, 1 min, 5 min, 30 min,
/// 2 h. A delivery has `BACKOFF.len() + 1` attempts.
pub const BACKOFF: [i64; 5] = [10, 60, 300, 1800, 7200];
/// The longest `Retry-After` that the worker keeps, in seconds.
pub const MAX_RETRY_AFTER: i64 = 7200;
/// The failed attempts in a row that disable a trigger.
pub const MAX_FAILURES: i64 = 20;
/// The days that the prune keeps a delivery.
pub const KEEP_DAYS: i64 = 7;
/// The bytes of a response that a delivery keeps.
pub const SNIPPET: usize = 2048;
/// The bytes of the body that the API shows.
pub const BODY_SHOWN: usize = 4096;

/// One delivery, as the API shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Delivery {
    pub id: String,
    /// The trigger (schedule) ID.
    pub schedule: String,
    pub event_seq: Option<i64>,
    pub event_kind: String,
    /// A test send (`POST /api/schedules/{id}/test`).
    pub test: bool,
    /// The delivery that a redeliver copied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redelivery_of: Option<String>,
    pub url: String,
    pub method: String,
    pub content_type: String,
    /// The attempts so far.
    pub attempt: i64,
    /// `pending`, `ok`, `failed`, `giving_up`, or `dropped`.
    pub status: String,
    pub http_status: Option<i64>,
    pub duration_ms: Option<i64>,
    /// The start of the response body (redacted, at most 2 KiB).
    pub response: Option<String>,
    pub error: Option<String>,
    /// `dropped`: the events that the rate limit dropped in its minute.
    #[serde(skip_serializing_if = "is_zero")]
    pub dropped: i64,
    pub next_attempt_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// The start of the request body (at most 4 KiB).
    pub request: String,
    /// The size of the request body.
    pub request_bytes: usize,
    /// The whole body: for the worker, not the API.
    #[serde(skip)]
    pub body: String,
    /// The headers of the format (ntfy: `Title`, `Priority`, `Tags`).
    #[serde(skip)]
    pub format_headers: Vec<(String, String)>,
    /// The `data.cause` of the delivery events.
    #[serde(skip)]
    pub cause: Option<Value>,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

/// The start of `s`, at most `max` bytes, on a character boundary.
pub fn cut(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn row(r: &Row<'_>) -> rusqlite::Result<Delivery> {
    let body: String = r.get("body")?;
    let headers: String = r.get("format_headers")?;
    let cause: Option<String> = r.get("cause")?;
    Ok(Delivery {
        id: r.get("id")?,
        schedule: r.get("schedule")?,
        event_seq: r.get("event_seq")?,
        event_kind: r.get("event_kind")?,
        test: r.get("test")?,
        redelivery_of: r.get("redelivery_of")?,
        url: r.get("url")?,
        method: r.get("method")?,
        content_type: r.get("content_type")?,
        attempt: r.get("attempt")?,
        status: r.get("status")?,
        http_status: r.get("http_status")?,
        duration_ms: r.get("duration_ms")?,
        response: r.get("response")?,
        error: r.get("error")?,
        dropped: r.get("dropped")?,
        next_attempt_at: r.get("next_attempt_at")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        request: cut(&body, BODY_SHOWN),
        request_bytes: body.len(),
        body,
        format_headers: serde_json::from_str(&headers).unwrap_or_default(),
        cause: cause.and_then(|c| serde_json::from_str(&c).ok()),
    })
}

pub fn get(conn: &Connection, id: &str) -> ApiResult<Delivery> {
    conn.query_row("SELECT * FROM webhook_deliveries WHERE id = ?1", [id], row)
        .optional()?
        .ok_or_else(|| ApiError::not_found(format!("no webhook delivery `{id}`")))
}

/// The deliveries of a trigger, newest first. The cursor is a delivery ID.
pub fn list(conn: &Connection, schedule: &str, page: &Page) -> ApiResult<Paged<Delivery>> {
    let limit = page.limit();
    let mut stmt = conn.prepare(
        "SELECT * FROM webhook_deliveries WHERE schedule = ?1 AND (?2 = '' OR id < ?2)
         ORDER BY id DESC LIMIT ?3",
    )?;
    let rows = stmt
        .query_map(
            params![schedule, page.cursor().unwrap_or(""), (limit + 1) as i64],
            row,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Paged::from_rows(rows, limit, |d: &Delivery| d.id.clone()))
}

/// The `pending` deliveries of enabled triggers that are due at `now`,
/// oldest first. A delivery of a disabled trigger waits for the enable.
pub fn due(conn: &Connection, now: DateTime<Utc>, limit: usize) -> ApiResult<Vec<Delivery>> {
    let mut stmt = conn.prepare(
        "SELECT d.* FROM webhook_deliveries d JOIN schedules s ON s.id = d.schedule
         WHERE d.status = 'pending' AND d.next_attempt_at <= ?1 AND s.enabled = 1
         ORDER BY d.next_attempt_at, d.id LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![stamp(now), limit as i64], row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The time of the next due delivery of an enabled trigger, if any.
pub fn next_due(conn: &Connection) -> ApiResult<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT min(d.next_attempt_at) FROM webhook_deliveries d
             JOIN schedules s ON s.id = d.schedule
             WHERE d.status = 'pending' AND s.enabled = 1",
            [],
            |r| r.get(0),
        )
        .optional()?
        .flatten())
}

/// A new delivery.
#[derive(Debug, Clone)]
pub struct NewDelivery {
    pub schedule: String,
    pub event_seq: Option<i64>,
    pub event_kind: String,
    pub test: bool,
    pub redelivery_of: Option<String>,
    pub url: String,
    pub method: String,
    pub content_type: String,
    pub body: String,
    pub format_headers: Vec<(String, String)>,
    pub cause: Option<Value>,
}

/// Queue a delivery: `pending`, due at `now`. A test send is the same row,
/// which the caller sends at once.
pub fn enqueue(t: &Tx<'_>, n: &NewDelivery, now: DateTime<Utc>) -> ApiResult<Delivery> {
    let id = ulid::Ulid::new().to_string();
    let at = stamp(now);
    t.execute(
        "INSERT INTO webhook_deliveries (id, schedule, event_seq, event_kind, test,
             redelivery_of, url, method, content_type, body, format_headers, status, cause,
             next_attempt_at, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'pending', ?12, ?13, ?13, ?13)",
        params![
            id,
            n.schedule,
            n.event_seq,
            n.event_kind,
            n.test,
            n.redelivery_of,
            n.url,
            n.method,
            n.content_type,
            n.body,
            serde_json::to_string(&n.format_headers)?,
            n.cause.as_ref().map(Value::to_string),
            at,
        ],
    )?;
    get(t, &id)
}

/// A copy of delivery `id` as a new `pending` delivery, due now.
pub fn redeliver(t: &mut Tx<'_>, id: &str, now: DateTime<Utc>) -> ApiResult<Delivery> {
    let d = get(t, id)?;
    if d.status == "dropped" {
        return Err(ApiError::invalid(
            "a `dropped` row is a rate-limit note: it has no request to send again",
        ));
    }
    let new = enqueue(
        t,
        &NewDelivery {
            schedule: d.schedule.clone(),
            event_seq: d.event_seq,
            event_kind: d.event_kind.clone(),
            test: d.test,
            redelivery_of: Some(d.id.clone()),
            url: d.url.clone(),
            method: d.method.clone(),
            content_type: d.content_type.clone(),
            body: d.body.clone(),
            format_headers: d.format_headers.clone(),
            cause: d.cause.clone(),
        },
        now,
    )?;
    let s = schedules::get(t, &d.schedule)?;
    t.audit(
        "schedule.webhook.redeliver",
        &s.key(),
        s.project.as_deref(),
        json!({"id": s.id, "delivery": new.id, "of": d.id}),
    )?;
    Ok(new)
}

/// A rate-limit note: the `dropped` row of the current minute of
/// trigger `schedule` counts one more event. Returns the count.
pub fn note_dropped(
    t: &Tx<'_>,
    schedule: &str,
    url: &str,
    method: &str,
    now: DateTime<Utc>,
) -> ApiResult<i64> {
    let since = stamp(now - chrono::Duration::seconds(60));
    let open: Option<(String, i64)> = t
        .query_row(
            "SELECT id, dropped FROM webhook_deliveries
             WHERE schedule = ?1 AND status = 'dropped' AND created_at > ?2
             ORDER BY id DESC LIMIT 1",
            params![schedule, since],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let at = stamp(now);
    let n = match open {
        Some((id, n)) => {
            t.execute(
                "UPDATE webhook_deliveries SET dropped = ?2, updated_at = ?3,
                        error = ?4 WHERE id = ?1",
                params![id, n + 1, at, dropped_text(n + 1)],
            )?;
            n + 1
        }
        None => {
            let id = ulid::Ulid::new().to_string();
            t.execute(
                "INSERT INTO webhook_deliveries (id, schedule, url, method, content_type, body,
                     status, dropped, error, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, '', '', 'dropped', 1, ?5, ?6, ?6)",
                params![id, schedule, url, method, dropped_text(1), at],
            )?;
            1
        }
    };
    Ok(n)
}

fn dropped_text(n: i64) -> String {
    format!(
        "rate limit: {n} event{} dropped in this minute",
        if n == 1 { "" } else { "s" }
    )
}

/// How one attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// A `2xx` answer.
    Ok,
    /// A network error, a time-out, a `5xx`, or a `429`: try again.
    Retry,
    /// Any other answer (a `3xx`, a `4xx`), a refused URL, a missing
    /// secret: no retry.
    Fail,
}

/// One attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    pub class: Class,
    pub http_status: Option<u16>,
    pub duration_ms: i64,
    /// The start of the response body, before redaction.
    pub response: Option<String>,
    pub error: Option<String>,
    /// The `Retry-After` of the answer, in seconds.
    pub retry_after: Option<i64>,
}

/// The status and the next attempt after attempt number `attempt` (1 is
/// the first) ended with `class`. A test send never retries.
pub fn next_step(
    attempt: i64,
    class: Class,
    retry_after: Option<i64>,
    test: bool,
    now: DateTime<Utc>,
) -> (&'static str, Option<DateTime<Utc>>) {
    match class {
        Class::Ok => ("ok", None),
        Class::Fail => ("failed", None),
        Class::Retry if test => ("failed", None),
        Class::Retry => {
            let Some(wait) = usize::try_from(attempt - 1)
                .ok()
                .and_then(|i| BACKOFF.get(i).copied())
            else {
                return ("giving_up", None);
            };
            let wait = retry_after
                .map_or(wait, |r| wait.max(r.min(MAX_RETRY_AFTER)))
                .max(1);
            ("pending", Some(now + chrono::Duration::seconds(wait)))
        }
    }
}

/// Record attempt `a` of delivery `id`: the row, the trigger's failure
/// count, and a `webhook.delivery` event. The [`MAX_FAILURES`]th failed
/// attempt in a row disables the trigger and mails the operator. `secrets`
/// are the secret values that the attempt used: the response and the
/// error never show them. Returns the row.
pub fn record_attempt(
    t: &mut Tx<'_>,
    id: &str,
    a: &Attempt,
    secrets: &[String],
    now: DateTime<Utc>,
) -> ApiResult<Delivery> {
    let d = get(t, id)?;
    let attempt = d.attempt + 1;
    let (status, next) = next_step(attempt, a.class, a.retry_after, d.test, now);
    let scrub = |s: &str| -> String {
        let mut out = s.to_owned();
        for v in secrets.iter().filter(|v| v.len() >= 4) {
            out = out.replace(v.as_str(), "[secret]");
        }
        t.redact_string(out)
    };
    let response = a.response.as_deref().map(|r| cut(&scrub(r), SNIPPET));
    let error = a.error.as_deref().map(scrub);
    t.execute(
        "UPDATE webhook_deliveries SET attempt = ?2, status = ?3, http_status = ?4,
                duration_ms = ?5, response = ?6, error = ?7, next_attempt_at = ?8,
                updated_at = ?9
         WHERE id = ?1",
        params![
            id,
            attempt,
            status,
            a.http_status,
            a.duration_ms,
            response,
            error,
            next.map(stamp),
            stamp(now)
        ],
    )?;
    let d = get(t, id)?;
    // The trigger can be gone (deleted during the attempt): keep the row.
    let Ok(s) = schedules::get(t, &d.schedule) else {
        return Ok(d);
    };
    let (actor, cause_was) = (t.actor.clone(), t.cause.clone());
    t.actor = schedules::run_label(&s.id);
    t.cause = d.cause.clone();
    let result = after_attempt(t, &s, &d, a.class, now);
    t.actor = actor;
    t.cause = cause_was;
    result?;
    Ok(d)
}

fn after_attempt(
    t: &mut Tx<'_>,
    s: &Schedule,
    d: &Delivery,
    class: Class,
    now: DateTime<Utc>,
) -> ApiResult<()> {
    let mut summary = format!(
        "webhook {}: {}{}",
        s.key(),
        d.status,
        d.http_status.map(|h| format!(" ({h})")).unwrap_or_default()
    );
    if d.status == "pending" {
        summary = format!("{summary}, retry {}", d.attempt);
    }
    let mut draft = EventDraft::new(
        "webhook.delivery",
        summary,
        json!({"trigger": s.id, "name": s.name, "delivery": d.id, "status": d.status,
               "http_status": d.http_status, "attempt": d.attempt, "test": d.test,
               "event_seq": d.event_seq}),
    );
    if let Some(p) = &s.project {
        draft = draft.project(p.clone());
    }
    t.emit(draft)?;
    // A test send does not count toward the disable.
    if d.test {
        return Ok(());
    }
    if class == Class::Ok {
        t.execute(
            "UPDATE schedules SET webhook_failures = 0 WHERE id = ?1",
            [&s.id],
        )?;
        return Ok(());
    }
    t.execute(
        "UPDATE schedules SET webhook_failures = webhook_failures + 1 WHERE id = ?1",
        [&s.id],
    )?;
    let failures: i64 = t.query_row(
        "SELECT webhook_failures FROM schedules WHERE id = ?1",
        [&s.id],
        |r| r.get(0),
    )?;
    if failures >= MAX_FAILURES && s.enabled {
        let why = format!(
            "{failures} webhook attempts in a row failed; the last: {}",
            d.error.as_deref().unwrap_or("no answer")
        );
        let s = schedules::set_enabled(t, &s.id, false, Some(&why), now)?;
        mail::send(
            t,
            Outgoing {
                from: &Address::System,
                to: &Address::Operator,
                subject: &format!("Webhook trigger {} disabled", s.key()),
                body: &format!(
                    "The daemon disabled trigger {} ({}). {why}. Its pending deliveries wait. \
                     Fix the receiver or the URL, then run `terrarium schedule enable {}`; \
                     `terrarium schedule deliveries {}` shows the log.",
                    s.key(),
                    s.id,
                    s.id,
                    s.id
                ),
                thread: None,
                in_reply_to: None,
            },
        )?;
    }
    Ok(())
}

/// The failed attempts in a row of trigger `id`.
pub fn failures(conn: &Connection, id: &str) -> ApiResult<i64> {
    Ok(conn.query_row(
        "SELECT webhook_failures FROM schedules WHERE id = ?1",
        [id],
        |r| r.get(0),
    )?)
}

/// Set the failure count of trigger `id` to zero (an enable).
pub fn reset_failures(t: &Tx<'_>, id: &str) -> ApiResult<()> {
    t.execute(
        "UPDATE schedules SET webhook_failures = 0 WHERE id = ?1",
        [id],
    )?;
    Ok(())
}

/// Delete the deliveries older than [`KEEP_DAYS`]. Returns the rows
/// deleted.
pub fn prune(t: &Tx<'_>, now: DateTime<Utc>) -> ApiResult<usize> {
    let before = stamp(now - chrono::Duration::days(KEEP_DAYS));
    Ok(t.execute(
        "DELETE FROM webhook_deliveries WHERE created_at < ?1",
        [before],
    )?)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_backoff_and_retry_after() {
        let now = DateTime::parse_from_rfc3339("2026-11-03T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let secs = |n: i64, ra: Option<i64>| {
            let (st, at) = next_step(n, Class::Retry, ra, false, now);
            (st, at.map(|a| (a - now).num_seconds()))
        };
        assert_eq!(secs(1, None), ("pending", Some(10)));
        assert_eq!(secs(2, None), ("pending", Some(60)));
        assert_eq!(secs(3, None), ("pending", Some(300)));
        assert_eq!(secs(4, None), ("pending", Some(1800)));
        assert_eq!(secs(5, None), ("pending", Some(7200)));
        assert_eq!(secs(6, None), ("giving_up", None));
        // A longer Retry-After wins; a shorter one does not; it is capped.
        assert_eq!(secs(1, Some(120)), ("pending", Some(120)));
        assert_eq!(secs(2, Some(5)), ("pending", Some(60)));
        assert_eq!(secs(1, Some(99_999)), ("pending", Some(MAX_RETRY_AFTER)));
        assert_eq!(next_step(1, Class::Ok, None, false, now), ("ok", None));
        assert_eq!(
            next_step(1, Class::Fail, None, false, now),
            ("failed", None)
        );
        // A test send does not retry.
        assert_eq!(
            next_step(1, Class::Retry, None, true, now),
            ("failed", None)
        );
        assert_eq!(cut("héllo", 2), "h…");
        assert_eq!(cut("abc", 5), "abc");
    }
}
