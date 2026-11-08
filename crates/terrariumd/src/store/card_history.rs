//! Estimates from card move events. Review uses the last 20 completed
//! visits in this project, with at least three samples. Repeated visits
//! count separately. Only the latest 2000 project card moves are considered, bounding
//! query work. Open visits and malformed or backwards times do not count.

use chrono::{DateTime, Duration};
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::ApiResult;

/// Completed visits to a column role (falling back to its name for older
/// events), newest exit first. This query is shared with ETA calibration.
/// The SQL window is the latest 2000 project card moves. At most `limit`
/// completed visits are returned, excluding invalid timestamps.
pub fn completed_visits(
    conn: &Connection,
    project: &str,
    column: &str,
    limit: usize,
) -> ApiResult<Vec<Duration>> {
    let mut stmt = conn.prepare(
        "WITH recent AS (
           SELECT seq, card, at, payload FROM events WHERE project = ?1
             AND kind = 'card'
             AND CASE WHEN json_valid(payload) THEN json_extract(payload, '$.change') END = 'moved'
           ORDER BY seq DESC LIMIT 2000
         ), moves AS (
           SELECT seq, at, payload,
             lead(at) OVER (PARTITION BY card ORDER BY seq) AS left_at,
             lead(seq) OVER (PARTITION BY card ORDER BY seq) AS left_seq,
             lead(payload) OVER (PARTITION BY card ORDER BY seq) AS left_payload
           FROM recent
         ) SELECT at, left_at FROM moves
           WHERE coalesce(json_extract(payload, '$.to_role'), json_extract(payload, '$.to')) = ?2
             AND coalesce(json_extract(left_payload, '$.from_role'), json_extract(left_payload, '$.from')) = ?2
             AND coalesce(json_extract(left_payload, '$.to_role'), json_extract(left_payload, '$.to')) <> ?2
           ORDER BY left_seq DESC LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![project, column, limit as i64], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut samples = Vec::new();
    for row in rows {
        let (start, end) = row?;
        if let (Ok(start), Ok(end)) = (
            DateTime::parse_from_rfc3339(&start),
            DateTime::parse_from_rfc3339(&end),
        ) && end >= start
            && samples.len() < limit
        {
            samples.push(end - start);
        }
        if samples.len() == limit {
            break;
        }
    }
    Ok(samples)
}

/// Per-response cache, including projects without enough history. List renders
/// share one instance, so history is queried once per project, not per card.
#[derive(Default)]
pub struct ReviewEstimates(std::collections::HashMap<String, Option<Duration>>);

impl ReviewEstimates {
    pub fn eta(
        &mut self,
        conn: &Connection,
        project: &str,
        card: &str,
    ) -> ApiResult<Option<(String, String)>> {
        let median = match self.0.get(project) {
            Some(value) => *value,
            None => {
                let value = review_duration(conn, project)?;
                self.0.insert(project.to_owned(), value);
                value
            }
        };
        let Some(median) = median else {
            return Ok(None);
        };
        review_deadline(conn, card, median)
    }
}

fn review_duration(conn: &Connection, project: &str) -> ApiResult<Option<Duration>> {
    let mut samples = completed_visits(conn, project, super::board::REVIEW, 20)?;
    if samples.len() < 3 {
        return Ok(None);
    }
    samples.sort();
    let mid = samples.len() / 2;
    let median = if samples.len().is_multiple_of(2) {
        samples[mid - 1] + (samples[mid] - samples[mid - 1]) / 2
    } else {
        samples[mid]
    };
    Ok(Some(median))
}

fn review_deadline(
    conn: &Connection,
    card: &str,
    median: Duration,
) -> ApiResult<Option<(String, String)>> {
    let start: Option<String> = conn
        .query_row(
            "SELECT at FROM events WHERE card = ?1 AND kind = 'card'
           AND json_extract(payload, '$.change') = 'moved'
           AND coalesce(json_extract(payload, '$.to_role'), json_extract(payload, '$.to')) = ?2
         ORDER BY seq DESC LIMIT 1",
            params![card, super::board::REVIEW],
            |r| r.get(0),
        )
        .optional()?;
    Ok(start.and_then(|start| {
        let at = DateTime::parse_from_rfc3339(&start)
            .ok()?
            .checked_add_signed(median)?;
        Some((at.to_rfc3339(), start))
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use serde_json::json;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE events(seq INTEGER PRIMARY KEY, project TEXT, card TEXT, kind TEXT, at TEXT, payload TEXT);").unwrap();
        c
    }

    fn moved(c: &Connection, project: &str, card: &str, minute: u32, from: &str, to: &str) {
        c.execute(
            "INSERT INTO events(project,card,kind,at,payload) VALUES (?1,?2,'card',?3,?4)",
            params![
                project,
                card,
                format!("2026-11-08T00:{minute:02}:00Z"),
                json!({"change":"moved", "from":from,"to":to}).to_string()
            ],
        )
        .unwrap();
    }

    fn history(c: &Connection) {
        for (id, end) in [("a", 2), ("b", 4), ("c", 12)] {
            moved(c, "p", id, 0, "working", "review");
            moved(c, "p", id, end, "review", "accepted");
        }
        moved(c, "p", "open", 10, "working", "review");
    }

    #[test]
    fn review_median_starts_at_entry_and_resets_on_reentry() {
        let c = db();
        history(&c);
        assert_eq!(
            ReviewEstimates::default()
                .eta(&c, "p", "open")
                .unwrap()
                .unwrap()
                .0,
            "2026-11-08T00:14:00+00:00"
        );
        moved(&c, "p", "open", 10, "review", "working");
        moved(&c, "p", "open", 20, "working", "review");
        // Re-entry completes a zero-minute visit: {0, 2, 4, 12} has median 3.
        assert_eq!(
            ReviewEstimates::default()
                .eta(&c, "p", "open")
                .unwrap()
                .unwrap()
                .0,
            "2026-11-08T00:23:00+00:00"
        );
    }

    #[test]
    fn no_history_means_no_review_eta() {
        let c = db();
        assert!(
            ReviewEstimates::default()
                .eta(&c, "p", "open")
                .unwrap()
                .is_none()
        );
        history(&c);
        assert!(
            ReviewEstimates::default()
                .eta(&c, "other", "open")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn history_is_project_scoped_recent_and_bounded() {
        let c = db();
        history(&c);
        assert_eq!(
            completed_visits(&c, "p", "review", 2).unwrap(),
            vec![Duration::minutes(12), Duration::minutes(4)]
        );
        assert!(
            completed_visits(&c, "other", "review", 20)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn render_cache_reuses_history_and_sql_window_is_bounded() {
        let c = db();
        history(&c);
        let mut cache = ReviewEstimates::default();
        let first = cache.eta(&c, "p", "open").unwrap();
        // Non-card traffic must not evict review history from the window.
        c.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<2000) INSERT INTO events(project,kind,payload) SELECT 'p','other','{}' FROM n;").unwrap();
        c.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<2000) INSERT INTO events(project,kind,payload) SELECT 'p','card','{\"change\":\"comment\"}' FROM n;").unwrap();
        assert_eq!(
            ReviewEstimates::default().eta(&c, "p", "open").unwrap(),
            first
        );
        // New card moves evict history, but this response reuses its cached median.
        c.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<2000) INSERT INTO events(project,card,kind,payload) SELECT 'p','noise','card','{\"change\":\"moved\"}' FROM n;").unwrap();
        assert_eq!(cache.eta(&c, "p", "open").unwrap(), first);
        assert!(completed_visits(&c, "p", "review", 20).unwrap().is_empty());
        assert!(
            ReviewEstimates::default()
                .eta(&c, "p", "open")
                .unwrap()
                .is_none()
        );
    }
}
