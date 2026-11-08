//! Estimates from card move events. Review uses the last 20 completed
//! visits in this project, with at least three samples. Repeated visits
//! count separately. Open visits and malformed or backwards times do not count.

use chrono::{DateTime, Duration};
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::ApiResult;

/// A completed worker estimate. Raw guesses remain in the event log.
pub struct WorkSample {
    pub actual_ms: f64,
    pub guess_ms: f64,
    pub labels: Vec<String>,
}

/// Most recent completed estimates, excluding jobs, superseded guesses,
/// clears, open work and invalid timestamps. The first subsequent ETA or
/// move closes an estimate, but only a move supplies a completed sample.
pub fn work_samples(conn: &Connection, project: &str) -> ApiResult<Vec<WorkSample>> {
    let mut stmt = conn.prepare(
        "SELECT e.at, json_extract(e.payload, '$.eta_at'), x.at,
                coalesce(json_extract(e.payload, '$.labels'), c.labels, '[]')
         FROM events e JOIN events x ON x.seq = (
           SELECT min(n.seq) FROM events n WHERE n.card = e.card AND n.seq > e.seq
             AND n.kind = 'card' AND json_extract(n.payload, '$.change') IN ('eta','moved'))
         LEFT JOIN cards c ON c.id = e.card
         WHERE e.project = ?1 AND e.kind = 'card'
           AND json_extract(e.payload, '$.change') = 'eta'
           AND coalesce(json_extract(e.payload, '$.job'), 0) = 0
           AND json_extract(x.payload, '$.change') = 'moved'
         ORDER BY x.seq DESC",
    )?;
    let rows = stmt.query_map([project], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut samples = Vec::new();
    for row in rows {
        let (start, guess, end, labels) = row?;
        let Some(guess) = guess else { continue };
        if let (Ok(start), Ok(guess), Ok(end)) = (
            DateTime::parse_from_rfc3339(&start),
            DateTime::parse_from_rfc3339(&guess),
            DateTime::parse_from_rfc3339(&end),
        ) && guess > start
            && end > start
        {
            samples.push(WorkSample {
                actual_ms: (end - start).num_milliseconds() as f64,
                guess_ms: (guess - start).num_milliseconds() as f64,
                labels: serde_json::from_str(&labels).unwrap_or_default(),
            });
        }
    }
    Ok(samples)
}

/// Nearest-rank p70. Five completed cards are required for each history.
fn p70(mut values: Vec<f64>) -> Option<f64> {
    if values.len() < 5 {
        return None;
    }
    values.sort_by(f64::total_cmp);
    Some(values[(values.len() * 7).div_ceil(10) - 1])
}

pub fn calibrated_ms(samples: &[WorkSample], labels: &[String], guess: Option<f64>) -> Option<f64> {
    let ratios = p70(samples
        .iter()
        .take(30)
        .map(|s| s.actual_ms / s.guess_ms)
        .collect());
    let actual = p70(samples
        .iter()
        .filter(|s| {
            labels
                .iter()
                .any(|l| l.starts_with("type:") && s.labels.contains(l))
        })
        .take(30)
        .map(|s| s.actual_ms)
        .collect());
    match guess {
        Some(guess) => Some((guess * ratios? * actual?).sqrt()),
        None => actual,
    }
}

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
        c.execute_batch("CREATE TABLE events(seq INTEGER PRIMARY KEY, project TEXT, card TEXT, kind TEXT, at TEXT, payload TEXT); CREATE TABLE cards(id TEXT, labels TEXT);").unwrap();
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
    fn calibration_combines_ratio_and_type_history_or_uses_history_alone() {
        let samples: Vec<_> = (1..=10)
            .rev()
            .map(|n| WorkSample {
                actual_ms: f64::from(n) * 1000.0,
                guess_ms: 10000.0,
                labels: vec!["type:impl".into()],
            })
            .collect();
        let labels = vec!["type:impl".into()];
        assert_eq!(
            calibrated_ms(&samples, &labels, Some(10000.0)),
            Some(7000.0)
        );
        assert_eq!(calibrated_ms(&samples, &labels, None), Some(7000.0));
        assert_eq!(calibrated_ms(&samples[..4], &labels, Some(10000.0)), None);
        assert_eq!(
            calibrated_ms(&samples, &["type:small".into()], Some(10000.0)),
            None
        );
        assert_eq!(
            calibrated_ms(&samples, &labels, Some(40000.0)),
            Some(14000.0)
        );
        let mut recent = samples;
        recent.extend((0..40).map(|_| WorkSample {
            actual_ms: 1e9,
            guess_ms: 1.0,
            labels: labels.clone(),
        }));
        assert!(calibrated_ms(&recent, &labels, None).is_some());
    }

    #[test]
    fn worker_samples_ignore_open_superseded_job_and_other_project() {
        let c = db();
        for (card, project, job) in [("a", "p", false), ("b", "other", false), ("c", "p", true)] {
            for minute in [0, 1] {
                c.execute("INSERT INTO events(project,card,kind,at,payload) VALUES (?1,?2,'card',?3,?4)", params![project, card, format!("2026-11-08T00:0{minute}:00Z"), json!({"change":"eta","eta_at":"2026-11-08T00:10:00Z","job":job,"labels":["type:impl"]}).to_string()]).unwrap();
            }
            moved(&c, project, card, 5, "working", "review");
        }
        let samples = work_samples(&c, "p").unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].actual_ms, 240000.0);
        assert_eq!(samples[0].guess_ms, 540000.0);
        assert_eq!(samples[0].labels, vec!["type:impl"]);
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
