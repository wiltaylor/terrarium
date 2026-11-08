//! Estimates from card move events. Review uses the last 20 completed
//! visits in this project, with at least three samples. Repeated visits
//! count separately. Open visits and malformed or backwards times do not count.

use chrono::{DateTime, Duration};
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::ApiResult;

/// Completed visits to a column role (falling back to its name for older
/// events), newest exit first. This query is shared with ETA calibration.
pub fn completed_visits(
    conn: &Connection,
    project: &str,
    column: &str,
    limit: usize,
) -> ApiResult<Vec<Duration>> {
    let mut stmt = conn.prepare(
        "WITH moves AS (
           SELECT seq, at, payload,
             lead(at) OVER (PARTITION BY card ORDER BY seq) AS left_at,
             lead(seq) OVER (PARTITION BY card ORDER BY seq) AS left_seq,
             lead(payload) OVER (PARTITION BY card ORDER BY seq) AS left_payload
           FROM events WHERE project = ?1 AND kind = 'card'
             AND json_extract(payload, '$.change') = 'moved'
         ) SELECT at, left_at FROM moves
           WHERE coalesce(json_extract(payload, '$.to_role'), json_extract(payload, '$.to')) = ?2
             AND coalesce(json_extract(left_payload, '$.from_role'), json_extract(left_payload, '$.from')) = ?2
             AND coalesce(json_extract(left_payload, '$.to_role'), json_extract(left_payload, '$.to')) <> ?2
           ORDER BY left_seq DESC",
    )?;
    let rows = stmt.query_map(params![project, column], |r| {
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

/// Derived review deadline and start, never persisted as a worker estimate.
/// An open merge job supersedes review, including while queued.
pub fn review_eta(
    conn: &Connection,
    project: &str,
    card: &str,
    merge_open: bool,
) -> ApiResult<Option<(String, String)>> {
    if merge_open {
        return Ok(None);
    }
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
    let start: Option<String> = conn.query_row(
        "SELECT at FROM events WHERE card = ?1 AND kind = 'card'
           AND json_extract(payload, '$.change') = 'moved'
           AND coalesce(json_extract(payload, '$.to_role'), json_extract(payload, '$.to')) = 'review'
         ORDER BY seq DESC LIMIT 1",
        [card], |r| r.get(0),
    ).optional()?;
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
            review_eta(&c, "p", "open", false).unwrap().unwrap().0,
            "2026-11-08T00:14:00+00:00"
        );
        moved(&c, "p", "open", 10, "review", "working");
        moved(&c, "p", "open", 20, "working", "review");
        // Even sample median: (2 + 4) / 2 = 3 minutes.
        assert_eq!(
            review_eta(&c, "p", "open", false).unwrap().unwrap().0,
            "2026-11-08T00:23:00+00:00"
        );
    }

    #[test]
    fn no_history_or_merge_means_no_review_eta() {
        let c = db();
        assert!(review_eta(&c, "p", "open", false).unwrap().is_none());
        history(&c);
        assert!(review_eta(&c, "p", "open", true).unwrap().is_none());
        assert!(review_eta(&c, "other", "open", false).unwrap().is_none());
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
}
