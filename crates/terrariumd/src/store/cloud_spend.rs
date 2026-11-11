//! The USD spend of Claude Code sessions, for the monthly budget of a cloud
//! subscription (`terrarium_core::cloud`, TER-630).
//!
//! Claude Code writes the running `totalCostUSD` of a session to its
//! session file (`cost-state`). `cloud_spend` keeps one row per session and
//! UTC month: `total_usd` is the highest total seen in the month, and
//! `base_usd` the total at the end of the months before. The spend of the
//! month is `total_usd - base_usd`, summed over the sessions. A total is a
//! running value, so a tail that reads a session file again writes the
//! same number and nothing counts twice.
//!
//! Only sessions on a cloud subscription get rows. [`prune`] removes the
//! months that are more than [`KEEP_MONTHS`] old.
//!
//! `costUSD` is Claude Code's price of the tokens at the list price, not
//! the bill of the cloud account (`docs/compat.md`, "Session file and
//! usage accounting").

use rusqlite::{Connection, OptionalExtension, params};

use crate::db::{Tx, now};
use crate::error::ApiResult;

/// Record the running total `total_usd` of session `session` of `agent`
/// on `subscription`, seen in `month` (`2026-11`).
pub fn record(
    t: &mut Tx<'_>,
    agent: &str,
    session: &str,
    subscription: Option<&str>,
    month: &str,
    total_usd: f64,
) -> ApiResult<()> {
    if !total_usd.is_finite() || total_usd < 0.0 || session.is_empty() {
        return Ok(());
    }
    let base: f64 = t
        .query_row(
            "SELECT max(total_usd) FROM cloud_spend
             WHERE agent = ?1 AND session = ?2 AND month < ?3",
            params![agent, session, month],
            |r| r.get::<_, Option<f64>>(0),
        )
        .optional()?
        .flatten()
        .unwrap_or(0.0);
    t.execute(
        "INSERT INTO cloud_spend (agent, session, month, subscription, base_usd, total_usd,
                                  updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, max(?5, ?6), ?7)
         ON CONFLICT (agent, session, month) DO UPDATE SET
             total_usd = max(cloud_spend.total_usd, excluded.total_usd),
             subscription = coalesce(excluded.subscription, cloud_spend.subscription),
             updated_at = excluded.updated_at",
        params![agent, session, month, subscription, base, total_usd, now()],
    )?;
    Ok(())
}

/// The USD spend of `subscription` in `month`.
pub fn month_spend(conn: &Connection, subscription: &str, month: &str) -> ApiResult<f64> {
    Ok(conn
        .query_row(
            "SELECT coalesce(sum(max(total_usd - base_usd, 0)), 0) FROM cloud_spend
             WHERE subscription = ?1 AND month = ?2",
            params![subscription, month],
            |r| r.get::<_, f64>(0),
        )
        .optional()?
        .unwrap_or(0.0))
}

/// The months of spend that stay: the current one and the 12 before.
pub const KEEP_MONTHS: u32 = 13;

/// Remove the rows of the months before `oldest` (`2025-11`). Returns the
/// rows removed.
pub fn prune(t: &mut Tx<'_>, oldest: &str) -> ApiResult<usize> {
    Ok(t.execute("DELETE FROM cloud_spend WHERE month < ?1", [oldest])?)
}

/// The oldest month that [`prune`] keeps at `now`: [`KEEP_MONTHS`] months
/// with the month of `now`.
pub fn oldest_kept(now: chrono::DateTime<chrono::Utc>) -> String {
    use chrono::Datelike;
    let months = i64::from(now.year()) * 12 + i64::from(now.month0()) - i64::from(KEEP_MONTHS - 1);
    format!(
        "{:04}-{:02}",
        months.div_euclid(12),
        months.rem_euclid(12) + 1
    )
}

/// The IDs of the agents on `subscription` (index `agents_subscription`).
pub fn agents_on(conn: &Connection, subscription: &str) -> ApiResult<Vec<String>> {
    let mut stmt = conn.prepare("SELECT id FROM agents WHERE subscription = ?1 ORDER BY id")?;
    let rows = stmt.query_map([subscription], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::events::EventHub;

    fn db() -> Db {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut conn).unwrap();
        Db::new(conn, EventHub::default())
    }

    async fn spend(db: &Db, sub: &str, month: &str) -> f64 {
        let (s, m) = (sub.to_owned(), month.to_owned());
        db.read(move |c| month_spend(c, &s, &m)).await.unwrap()
    }

    /// Running totals: a repeat counts once, a lower total (an older line
    /// read again) changes nothing, and a session that runs into the next
    /// month counts there only from the total at the month's start.
    #[tokio::test]
    async fn running_totals_per_session_and_month() {
        let db = db();
        db.write("t", |t| {
            record(t, "a1", "s1", Some("aws"), "2026-11", 1.5)?;
            record(t, "a1", "s1", Some("aws"), "2026-11", 1.5)?;
            record(t, "a1", "s1", Some("aws"), "2026-11", 4.0)?;
            record(t, "a1", "s1", Some("aws"), "2026-11", 2.0)?;
            record(t, "a2", "s9", Some("aws"), "2026-11", 3.0)?;
            record(t, "a3", "s7", Some("max"), "2026-11", 9.0)?;
            record(t, "a1", "s1", Some("aws"), "2026-11", f64::NAN)?;
            record(t, "a1", "", Some("aws"), "2026-11", 5.0)
        })
        .await
        .unwrap();
        assert_eq!(spend(&db, "aws", "2026-11").await, 7.0);
        assert_eq!(spend(&db, "max", "2026-11").await, 9.0);
        db.write("t", |t| record(t, "a1", "s1", Some("aws"), "2026-12", 6.5))
            .await
            .unwrap();
        assert_eq!(spend(&db, "aws", "2026-12").await, 2.5);
        assert_eq!(spend(&db, "aws", "2026-11").await, 7.0);
        assert_eq!(spend(&db, "aws", "2027-01").await, 0.0);
        // The prune keeps 13 months.
        let n = db.write("t", |t| prune(t, "2026-12")).await.unwrap();
        assert_eq!(n, 3);
        assert_eq!(spend(&db, "aws", "2026-12").await, 2.5);
        assert_eq!(spend(&db, "aws", "2026-11").await, 0.0);
    }

    #[test]
    fn the_oldest_kept_month() {
        let at = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s)
                .unwrap()
                .with_timezone(&chrono::Utc)
        };
        assert_eq!(oldest_kept(at("2026-11-10T00:00:00Z")), "2025-11");
        assert_eq!(oldest_kept(at("2026-01-31T00:00:00Z")), "2025-01");
        assert_eq!(oldest_kept(at("2026-12-01T00:00:00Z")), "2025-12");
    }

    /// The query of [`agents_on`] uses its index.
    #[tokio::test]
    async fn agents_on_uses_the_index() {
        let db = db();
        let plan: Vec<String> = db
            .read(|c| {
                let mut stmt = c.prepare(
                    "EXPLAIN QUERY PLAN SELECT id FROM agents WHERE subscription = ?1 ORDER BY id",
                )?;
                let rows = stmt.query_map(["aws"], |r| r.get::<_, String>(3))?;
                Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })
            .await
            .unwrap();
        assert!(
            plan.iter().any(|p| p.contains("agents_subscription")),
            "{plan:?}"
        );
    }
}
