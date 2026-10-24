//! Local usage accounting, quota holds, and sent alerts (PLAN §16.2).
//!
//! `usage_samples` has one row per API response of an agent. The unique
//! index on `(agent, message_id)` is the dedupe rule of `docs/compat.md`:
//! a response that the session file repeats, or that a tail reads again
//! after a restart, counts once.

use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use serde::{Deserialize, Serialize};

use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};

/// One usage sample to store.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct NewUsage {
    pub agent: String,
    pub project: String,
    pub subscription: Option<String>,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub message_id: String,
    pub turn_seq: Option<i64>,
    pub session: Option<String>,
    /// RFC 3339. The time of the response, else now.
    pub at: Option<String>,
}

fn i(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// Insert samples. A sample whose `(agent, message_id)` is stored already
/// is skipped. Returns the samples that were new.
pub fn insert(t: &mut Tx<'_>, samples: &[NewUsage]) -> ApiResult<Vec<NewUsage>> {
    let mut added = Vec::new();
    for s in samples {
        if s.message_id.is_empty() {
            continue;
        }
        let n = t.execute(
            "INSERT OR IGNORE INTO usage_samples
                 (agent, project, subscription, model, input_tokens, output_tokens, cache_read,
                  cache_write, message_id, turn_seq, session, at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                s.agent,
                s.project,
                s.subscription,
                s.model,
                i(s.input_tokens),
                i(s.output_tokens),
                i(s.cache_read),
                i(s.cache_write),
                s.message_id,
                s.turn_seq,
                s.session,
                s.at.clone().unwrap_or_else(now),
            ],
        )?;
        if n == 1 {
            added.push(s.clone());
        }
    }
    Ok(added)
}

/// Token sums.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Totals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// API responses.
    pub messages: u64,
    /// Distinct turns.
    pub turns: u64,
}

impl Totals {
    /// Input plus output tokens: the number that budgets and shares use.
    /// Cache reads are cheap and would hide the real spend.
    pub fn tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    pub fn add(&mut self, s: &NewUsage) {
        self.input_tokens += s.input_tokens;
        self.output_tokens += s.output_tokens;
        self.cache_read += s.cache_read;
        self.cache_write += s.cache_write;
        self.messages += 1;
    }
}

const SUMS: &str = "coalesce(sum(input_tokens), 0) AS input_tokens,
     coalesce(sum(output_tokens), 0) AS output_tokens,
     coalesce(sum(cache_read), 0) AS cache_read,
     coalesce(sum(cache_write), 0) AS cache_write,
     count(*) AS messages,
     count(DISTINCT agent || ':' || coalesce(turn_seq, '')) AS turns";

fn u(r: &Row<'_>, col: &str) -> rusqlite::Result<u64> {
    let v: i64 = r.get(col)?;
    Ok(u64::try_from(v).unwrap_or(0))
}

fn totals_row(r: &Row<'_>) -> rusqlite::Result<Totals> {
    Ok(Totals {
        input_tokens: u(r, "input_tokens")?,
        output_tokens: u(r, "output_tokens")?,
        cache_read: u(r, "cache_read")?,
        cache_write: u(r, "cache_write")?,
        messages: u(r, "messages")?,
        turns: u(r, "turns")?,
    })
}

/// Which samples a query covers.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UsageFilter {
    pub subscription: Option<String>,
    pub project: Option<String>,
    /// An agent ULID.
    pub agent: Option<String>,
    pub model: Option<String>,
    /// RFC 3339, inclusive.
    pub since: Option<String>,
    /// RFC 3339, exclusive.
    pub until: Option<String>,
}

impl UsageFilter {
    fn sql(&self) -> (String, Vec<String>) {
        let mut sql = String::from(" WHERE 1 = 1");
        let mut args = Vec::new();
        for (col, val) in [
            ("subscription", &self.subscription),
            ("project", &self.project),
            ("agent", &self.agent),
            ("model", &self.model),
        ] {
            if let Some(v) = val {
                sql.push_str(&format!(" AND {col} = ?"));
                args.push(v.clone());
            }
        }
        if let Some(s) = &self.since {
            sql.push_str(" AND at >= ?");
            args.push(s.clone());
        }
        if let Some(s) = &self.until {
            sql.push_str(" AND at < ?");
            args.push(s.clone());
        }
        (sql, args)
    }
}

pub fn totals(conn: &Connection, f: &UsageFilter) -> ApiResult<Totals> {
    let (filter, args) = f.sql();
    let sql = format!("SELECT {SUMS} FROM usage_samples{filter}");
    Ok(conn.query_row(&sql, params_from_iter(args), totals_row)?)
}

/// The bucket size of a series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Bucket {
    #[default]
    Hour,
    Day,
}

impl Bucket {
    /// The SQLite expression for the start of the bucket of `at`.
    fn expr(self) -> &'static str {
        match self {
            // `at` is RFC 3339 with a `Z`; the first 13 characters are the hour.
            Self::Hour => "substr(at, 1, 13) || ':00:00Z'",
            Self::Day => "substr(at, 1, 10) || 'T00:00:00Z'",
        }
    }
}

/// A column to split a series by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupBy {
    Subscription,
    Project,
    Agent,
    Model,
}

impl GroupBy {
    fn col(self) -> &'static str {
        match self {
            Self::Subscription => "subscription",
            Self::Project => "project",
            Self::Agent => "agent",
            Self::Model => "model",
        }
    }
}

/// One point of a usage series.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsagePoint {
    /// The start of the bucket.
    pub bucket: String,
    /// The value of the group column, when the series is split.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(flatten)]
    pub totals: Totals,
}

/// A usage series: one point per bucket (and per group value), oldest
/// first.
pub fn series(
    conn: &Connection,
    f: &UsageFilter,
    bucket: Bucket,
    group: Option<GroupBy>,
) -> ApiResult<Vec<UsagePoint>> {
    let (filter, args) = f.sql();
    let key = group.map_or("NULL", GroupBy::col);
    let sql = format!(
        "SELECT {b} AS bucket, {key} AS key, {SUMS} FROM usage_samples{filter}
         GROUP BY 1, 2 ORDER BY 1, 2",
        b = bucket.expr()
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(args), |r| {
        Ok(UsagePoint {
            bucket: r.get("bucket")?,
            key: r.get("key")?,
            totals: totals_row(r)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Totals per agent for a filter, biggest spender first.
pub fn by_agent(conn: &Connection, f: &UsageFilter) -> ApiResult<Vec<(String, Totals)>> {
    let (filter, args) = f.sql();
    let sql = format!(
        "SELECT agent, {SUMS} FROM usage_samples{filter}
         GROUP BY agent ORDER BY sum(input_tokens + output_tokens) DESC, agent"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(args), |r| {
        Ok((r.get::<_, String>("agent")?, totals_row(r)?))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The start of the current UTC day, RFC 3339.
pub fn today_start() -> String {
    format!("{}T00:00:00Z", chrono::Utc::now().format("%Y-%m-%d"))
}

/// A hold on an agent's new turns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hold {
    pub agent: String,
    /// `quota:<subscription>` or `budget:<project>`.
    pub cause: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    pub reason: String,
    /// True when the supervisor holds the turns. False: the supervisor
    /// cannot hold turns, so the hold is advice only.
    pub enforced: bool,
    pub created_at: String,
}

fn hold_row(r: &Row<'_>) -> rusqlite::Result<Hold> {
    Ok(Hold {
        agent: r.get("agent")?,
        cause: r.get("cause")?,
        until: r.get("until")?,
        reason: r.get("reason")?,
        enforced: r.get::<_, i64>("enforced")? != 0,
        created_at: r.get("created_at")?,
    })
}

/// Add a hold. Returns false when the agent has this hold already.
pub fn add_hold(
    t: &mut Tx<'_>,
    agent: &str,
    cause: &str,
    until: Option<&str>,
    reason: &str,
    enforced: bool,
) -> ApiResult<bool> {
    let n = t.execute(
        "INSERT OR IGNORE INTO quota_holds (agent, cause, until, reason, enforced, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![agent, cause, until, reason, enforced, now()],
    )?;
    Ok(n == 1)
}

pub fn remove_hold(t: &mut Tx<'_>, agent: &str, cause: &str) -> ApiResult<bool> {
    Ok(t.execute(
        "DELETE FROM quota_holds WHERE agent = ?1 AND cause = ?2",
        params![agent, cause],
    )? == 1)
}

pub fn holds(conn: &Connection) -> ApiResult<Vec<Hold>> {
    let mut stmt = conn.prepare("SELECT * FROM quota_holds ORDER BY created_at, agent")?;
    let rows = stmt.query_map([], hold_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn holds_of(conn: &Connection, agent: &str) -> ApiResult<Vec<Hold>> {
    let mut stmt = conn.prepare("SELECT * FROM quota_holds WHERE agent = ?1 ORDER BY cause")?;
    let rows = stmt.query_map([agent], hold_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Record an alert. Returns false when this alert was sent already.
pub fn mark_alert(
    t: &mut Tx<'_>,
    scope: &str,
    window: &str,
    period: &str,
    level: &str,
) -> ApiResult<bool> {
    let n = t.execute(
        "INSERT OR IGNORE INTO quota_alerts (scope, window, period, level, at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![scope, window, period, level, now()],
    )?;
    Ok(n == 1)
}

/// The agents on a subscription: `(id, name, project)`.
pub fn agents_on(
    conn: &Connection,
    subscription: &str,
) -> ApiResult<Vec<(String, String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, project FROM agents WHERE subscription = ?1 ORDER BY project, name",
    )?;
    let rows = stmt.query_map([subscription], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The agents of a project, optionally only those on one subscription.
pub fn agents_of(
    conn: &Connection,
    project: &str,
    subscription: Option<&str>,
) -> ApiResult<Vec<(String, String, Option<String>)>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, subscription FROM agents
         WHERE project = ?1 AND (?2 IS NULL OR subscription = ?2) ORDER BY name",
    )?;
    let rows = stmt.query_map(params![project, subscription], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The `seq` of the newest `turn_start` event of an agent.
pub fn last_turn_seq(conn: &Connection, agent: &str) -> ApiResult<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT max(seq) FROM events WHERE agent_id = ?1 AND kind = 'turn_start'",
            [agent],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten())
}

/// Parse an RFC 3339 query value, for a clear error.
pub fn check_time(what: &str, v: &str) -> ApiResult<String> {
    chrono::DateTime::parse_from_rfc3339(v)
        .map(|t| {
            t.with_timezone(&chrono::Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        })
        .map_err(|_| ApiError::bad_request(format!("`{what}` is not an RFC 3339 time: `{v}`")))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::events::EventHub;

    fn db() -> Db {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut conn).unwrap();
        Db::new(conn, EventHub::default())
    }

    fn s(agent: &str, msg: &str, input: u64, at: &str) -> NewUsage {
        NewUsage {
            agent: agent.into(),
            project: "p".into(),
            subscription: Some("sub".into()),
            model: "claude-haiku-5-5".into(),
            input_tokens: input,
            output_tokens: 10,
            cache_read: 100,
            cache_write: 5,
            message_id: msg.into(),
            turn_seq: Some(1),
            session: None,
            at: Some(at.into()),
        }
    }

    #[tokio::test]
    async fn dedupe_by_agent_and_message_id() {
        let db = db();
        let first = vec![
            s("A", "m1", 2, "2026-10-24T06:10:00.000Z"),
            s("A", "m1", 2, "2026-10-24T06:10:00.000Z"),
            s("A", "m2", 3, "2026-10-24T07:10:00.000Z"),
            // Another agent may see the same id (a copied session).
            s("B", "m1", 4, "2026-10-24T07:20:00.000Z"),
        ];
        let added = db
            .write("system", move |t| insert(t, &first))
            .await
            .unwrap();
        assert_eq!(added.len(), 3);
        // A tail that reads the file again adds nothing.
        let again = vec![s("A", "m1", 2, "2026-10-24T06:10:00.000Z")];
        let added = db
            .write("system", move |t| insert(t, &again))
            .await
            .unwrap();
        assert!(added.is_empty());

        let all = db
            .read(|c| totals(c, &UsageFilter::default()))
            .await
            .unwrap();
        assert_eq!(all.input_tokens, 9);
        assert_eq!(all.messages, 3);
        assert_eq!(all.turns, 2);
        assert_eq!(all.tokens(), 39);
        let a = db
            .read(|c| {
                totals(
                    c,
                    &UsageFilter {
                        agent: Some("A".into()),
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap();
        assert_eq!(a.input_tokens, 5);

        let hours = db
            .read(|c| series(c, &UsageFilter::default(), Bucket::Hour, None))
            .await
            .unwrap();
        assert_eq!(hours.len(), 2);
        assert_eq!(hours[0].bucket, "2026-10-24T06:00:00Z");
        assert_eq!(hours[1].totals.input_tokens, 7);
        let split = db
            .read(|c| {
                series(
                    c,
                    &UsageFilter::default(),
                    Bucket::Day,
                    Some(GroupBy::Agent),
                )
            })
            .await
            .unwrap();
        assert_eq!(split.len(), 2);
        assert_eq!(split[0].key.as_deref(), Some("A"));
        let ranked = db
            .read(|c| by_agent(c, &UsageFilter::default()))
            .await
            .unwrap();
        assert_eq!(ranked[0].0, "A");
        let since = db
            .read(|c| {
                totals(
                    c,
                    &UsageFilter {
                        since: Some("2026-10-24T07:00:00.000Z".into()),
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap();
        assert_eq!(since.messages, 2);
    }

    #[tokio::test]
    async fn holds_and_alerts_are_idempotent() {
        let db = db();
        let (a, b, c) = db
            .write("system", |t| {
                let a = add_hold(t, "A", "quota:s", Some("2026-10-24T10:00:00Z"), "5h", true)?;
                let b = add_hold(t, "A", "quota:s", None, "again", true)?;
                let c = mark_alert(t, "s", "5h", "p1", "alert")?
                    && !mark_alert(t, "s", "5h", "p1", "alert")?;
                Ok((a, b, c))
            })
            .await
            .unwrap();
        assert!(a && !b && c);
        let h = db.read(|c| holds_of(c, "A")).await.unwrap();
        assert_eq!(h.len(), 1);
        assert!(h[0].enforced);
        assert!(
            db.write("system", |t| remove_hold(t, "A", "quota:s"))
                .await
                .unwrap()
        );
        assert!(db.read(holds).await.unwrap().is_empty());
        assert!(check_time("since", "nope").is_err());
        assert_eq!(
            check_time("since", "2026-10-24T10:00:00+10:00").unwrap(),
            "2026-10-24T00:00:00.000Z"
        );
    }
}
