//! Estimates from card move events. Review uses the last 20 completed
//! visits in this project, with at least three samples. Repeated visits
//! count separately. Only the latest 2000 project card moves are considered, bounding
//! query work. Open visits and malformed or backwards times do not count.

use chrono::{DateTime, Duration};
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::ApiResult;

/// A completed worker estimate. Raw guesses remain in the event log.
#[derive(Clone)]
pub struct WorkSample {
    pub card: String,
    pub start_seq: i64,
    pub end_seq: i64,
    pub actual_ms: f64,
    pub guess_ms: f64,
    pub labels: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WorkHistory {
    pub n: usize,
    pub median_ms: Option<f64>,
    pub p70_ms: Option<f64>,
}

/// The same latest 30 distinct cards and type matching as worker calibration.
fn work_history(samples: &[WorkSample], labels: &[String]) -> WorkHistory {
    let mut seen = std::collections::HashSet::new();
    let mut values: Vec<_> = samples
        .iter()
        .filter(|s| seen.insert(&s.card))
        .filter(|s| {
            labels
                .iter()
                .any(|l| l.starts_with("type:") && s.labels.contains(l))
        })
        .take(30)
        .map(|s| s.actual_ms)
        .collect();
    values.sort_by(f64::total_cmp);
    let n = values.len();
    let median_ms = (n > 0).then(|| {
        if n % 2 == 0 {
            (values[n / 2 - 1] + values[n / 2]) / 2.0
        } else {
            values[n / 2]
        }
    });
    WorkHistory {
        n,
        median_ms,
        p70_ms: p70(values, 1),
    }
}
const WORK_SAMPLES_SQL: &str = "WITH recent AS (
           SELECT DISTINCT card FROM (
             SELECT card FROM events WHERE project = ?1
               AND kind = 'card'
               AND CASE WHEN json_valid(payload) THEN json_extract(payload, '$.change') END = 'moved'
             ORDER BY seq DESC LIMIT 2000)
         ), steps AS (
           SELECT e.seq, e.card, e.project, e.at, e.payload,
             json_extract(e.payload, '$.change') AS change,
             lead(e.seq) OVER w AS next_seq,
             lead(e.at) OVER w AS next_at,
             lead(json_extract(e.payload, '$.change')) OVER w AS next_change
           FROM events e
           WHERE e.card IN (SELECT card FROM recent) AND e.kind = 'card'
             AND CASE WHEN json_valid(e.payload) THEN json_extract(e.payload, '$.change') END IN ('eta', 'moved')
           WINDOW w AS (PARTITION BY e.card ORDER BY e.seq)
         )
         SELECT s.at, json_extract(s.payload, '$.eta_at'), s.next_at,
                coalesce(json_extract(s.payload, '$.labels'), c.labels, '[]'), s.seq, s.next_seq, s.card
         FROM steps s LEFT JOIN cards c ON c.id = s.card
         WHERE s.project = ?1 AND s.change = 'eta' AND s.next_change = 'moved'
           AND coalesce(json_extract(s.payload, '$.column_role'), json_extract(s.payload, '$.column')) IN ('working', 'in_progress')
           AND coalesce(json_extract(s.payload, '$.job'), 0) = 0
         ORDER BY s.next_seq DESC";

/// Most recent completed estimates, excluding jobs, superseded guesses,
/// clears, open work and invalid timestamps. The first subsequent ETA or
/// move closes an estimate, but only a move supplies a completed sample.
///
/// Bounded (TER-394): only cards with one of the latest 2000 project card
/// moves (the `events_card_moves` index) count, and their ETA and move
/// events come by `events_card`. A window function pairs each ETA with the
/// next ETA or move, so no subquery runs per event. The query runs under
/// the database lock on every card list, so it must not scan the event log.
pub fn work_samples(conn: &Connection, project: &str) -> ApiResult<Vec<WorkSample>> {
    let mut stmt = conn.prepare(WORK_SAMPLES_SQL)?;
    let rows = stmt.query_map([project], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, String>(6)?,
        ))
    })?;
    let mut samples = Vec::new();
    for row in rows {
        let (start, guess, end, labels, start_seq, end_seq, card) = row?;
        let Some(guess) = guess else { continue };
        if let (Ok(start), Ok(guess), Ok(end)) = (
            DateTime::parse_from_rfc3339(&start),
            DateTime::parse_from_rfc3339(&guess),
            DateTime::parse_from_rfc3339(&end),
        ) && guess > start
            && end > start
        {
            samples.push(WorkSample {
                card,
                start_seq,
                end_seq,
                actual_ms: (end - start).num_milliseconds() as f64,
                guess_ms: (guess - start).num_milliseconds() as f64,
                labels: serde_json::from_str(&labels).unwrap_or_default(),
            });
        }
    }
    Ok(samples)
}

/// Nearest-rank p70 with the caller's minimum sample size.
fn p70(mut values: Vec<f64>, minimum: usize) -> Option<f64> {
    if values.is_empty() || values.len() < minimum {
        return None;
    }
    values.sort_by(f64::total_cmp);
    Some(values[(values.len() * 7).div_ceil(10) - 1])
}

pub fn calibrated_ms(samples: &[WorkSample], labels: &[String], guess: Option<f64>) -> Option<f64> {
    let mut seen = std::collections::HashSet::new();
    let ratios = p70(
        samples
            .iter()
            .filter(|s| seen.insert(&s.card))
            .take(30)
            .map(|s| s.actual_ms / s.guess_ms)
            .collect(),
        5,
    );
    seen.clear();
    let actual = p70(
        samples
            .iter()
            .filter(|s| seen.insert(&s.card))
            .filter(|s| {
                labels
                    .iter()
                    .any(|l| l.starts_with("type:") && s.labels.contains(l))
            })
            .take(30)
            .map(|s| s.actual_ms)
            .collect(),
        5,
    );
    match guess {
        Some(guess) => Some((guess * ratios? * actual?).sqrt()),
        None => actual,
    }
}

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
pub struct ReviewEstimates {
    review: std::collections::HashMap<String, Option<Duration>>,
    work: std::collections::HashMap<String, Vec<WorkSample>>,
    work_failed: std::collections::HashSet<String>,
}

impl ReviewEstimates {
    fn load_work(&mut self, conn: &Connection, project: &str) -> bool {
        if self.work_failed.contains(project) {
            return false;
        }
        if !self.work.contains_key(project) {
            match work_samples(conn, project) {
                Ok(samples) => {
                    self.work.insert(project.to_owned(), samples);
                }
                Err(error) => {
                    tracing::warn!(%project, %error, "cannot load similar-card history");
                    self.work_failed.insert(project.to_owned());
                    return false;
                }
            }
        }
        true
    }

    pub fn history(
        &mut self,
        conn: &Connection,
        project: &str,
        labels: &[String],
    ) -> Option<WorkHistory> {
        if !self.load_work(conn, project) {
            return None;
        }
        Some(work_history(&self.work[project], labels))
    }

    pub fn worker_ms(
        &mut self,
        conn: &Connection,
        project: &str,
        labels: &[String],
        guess: Option<f64>,
    ) -> ApiResult<Option<f64>> {
        if !self.load_work(conn, project) {
            return Ok(None);
        }
        Ok(self
            .work
            .get(project)
            .and_then(|s| calibrated_ms(s, labels, guess)))
    }

    pub fn eta(
        &mut self,
        conn: &Connection,
        project: &str,
        card: &str,
    ) -> ApiResult<Option<(String, String)>> {
        let median = match self.review.get(project) {
            Some(value) => *value,
            None => {
                let value = review_duration(conn, project)?;
                self.review.insert(project.to_owned(), value);
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

    #[test]
    fn similar_history_uses_latest_distinct_same_type_cards() {
        let samples: Vec<_> = (1..=40)
            .map(|n| WorkSample {
                card: n.to_string(),
                start_seq: n,
                end_seq: n + 1,
                actual_ms: n as f64,
                guess_ms: 100.0,
                labels: vec!["type:small".into()],
            })
            .collect();
        let labels = vec!["type:small".into()];
        let h = work_history(&samples, &labels);
        assert_eq!(h.n, 30);
        assert_eq!(h.median_ms, Some(15.5));
        assert_eq!(h.p70_ms, Some(21.0));
        let mut repeats = vec![samples[0].clone(); 5];
        repeats.push(samples[1].clone());
        assert_eq!(work_history(&repeats, &labels).n, 2);
        let empty = work_history(&samples, &["type:impl".into()]);
        assert_eq!(empty.n, 0);
        assert_eq!(empty.median_ms, None);
        assert_eq!(empty.p70_ms, None);
        assert_eq!(work_history(&samples[..1], &labels).median_ms, Some(1.0));
    }

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
                card: n.to_string(),
                start_seq: 0,
                end_seq: 1,
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
        // Repeated visits of one card do not satisfy the five-card minimum.
        let repeats = vec![samples[0].clone(); 10];
        assert_eq!(calibrated_ms(&repeats, &labels, None), None);
        let mut recent: Vec<_> = (0..30)
            .map(|n| WorkSample {
                card: format!("new{n}"),
                ..samples[0].clone()
            })
            .collect();
        recent.extend((0..40).map(|n| WorkSample {
            card: format!("old{n}"),
            start_seq: 0,
            end_seq: 1,
            actual_ms: 1e9,
            guess_ms: 1.0,
            labels: labels.clone(),
        }));
        assert_eq!(calibrated_ms(&recent, &labels, None), Some(10000.0));
        assert_eq!(
            calibrated_ms(&recent, &labels, Some(10000.0)),
            Some(10000.0)
        );
    }

    #[test]
    fn worker_samples_ignore_open_superseded_job_and_other_project() {
        let c = db();
        for (card, project, job) in [("a", "p", false), ("b", "other", false), ("c", "p", true)] {
            for minute in [0, 1] {
                c.execute("INSERT INTO events(project,card,kind,at,payload) VALUES (?1,?2,'card',?3,?4)", params![project, card, format!("2026-11-08T00:0{minute}:00Z"), json!({"change":"eta","column_role":"working","eta_at":"2026-11-08T00:10:00Z","job":job,"labels":["type:impl"]}).to_string()]).unwrap();
            }
            moved(&c, project, card, 5, "working", "review");
        }
        let samples = work_samples(&c, "p").unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].actual_ms, 240000.0);
        assert_eq!(samples[0].guess_ms, 540000.0);
        assert_eq!(samples[0].labels, vec!["type:impl"]);
    }

    /// TER-394: worker samples come from the cards of the latest 2000
    /// project moves, through the move index, with no per-event subquery,
    /// and malformed payloads do not fail the read.
    #[test]
    fn worker_samples_are_bounded_by_recent_moves_and_use_the_index() {
        let c = db();
        c.execute_batch(
            "CREATE INDEX events_project ON events(project, seq);
             CREATE INDEX events_card ON events(card, seq);
             CREATE INDEX events_card_moves ON events(project, seq)
               WHERE kind = 'card'
                 AND CASE WHEN json_valid(payload) THEN json_extract(payload, '$.change') END = 'moved';",
        )
        .unwrap();
        const ETA: &str =
            r#"{"change":"eta","column_role":"working","eta_at":"2026-11-08T00:30:00Z"}"#;
        let eta = |card: &str, minute: u32| {
            c.execute(
                "INSERT INTO events(project,card,kind,at,payload) VALUES ('p',?1,'card',?2,?3)",
                params![card, format!("2026-11-08T00:{minute:02}:00Z"), ETA],
            )
            .unwrap();
        };
        eta("old", 0);
        moved(&c, "p", "old", 5, "working", "review");
        c.execute(
            "INSERT INTO events(project,card,kind,at,payload) VALUES ('p','old','card','x','not JSON')",
            [],
        )
        .unwrap();
        eta("new", 1);
        moved(&c, "p", "new", 6, "working", "review");
        assert_eq!(work_samples(&c, "p").unwrap().len(), 2);
        // 2000 newer moves of another card push `old` out of the window;
        // `new` stays because it moves again inside it.
        c.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1999) INSERT INTO events(project,card,kind,at,payload) SELECT 'p','noise','card','2026-11-08T00:07:00Z','{\"change\":\"moved\"}' FROM n;").unwrap();
        moved(&c, "p", "new", 8, "review", "accepted");
        let samples = work_samples(&c, "p").unwrap();
        assert_eq!(
            samples.iter().map(|s| s.card.as_str()).collect::<Vec<_>>(),
            vec!["new"]
        );
        assert_eq!(samples[0].actual_ms, 300000.0);
        let mut stmt = c
            .prepare(&format!("EXPLAIN QUERY PLAN {WORK_SAMPLES_SQL}"))
            .unwrap();
        let plan: Vec<String> = stmt
            .query_map(["p"], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            plan.iter().any(|p| p.contains("events_card_moves")),
            "{plan:?}"
        );
        assert!(!plan.iter().any(|p| p.contains("CORRELATED")), "{plan:?}");
    }

    /// Run against a read-API export, never the daemon's live database.
    #[test]
    #[ignore = "requires TERRARIUM_ETA_HISTORY export"]
    fn backtest_export() {
        let path = std::env::var("TERRARIUM_ETA_HISTORY").unwrap();
        let cards: Vec<serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let c = db();
        for card in &cards {
            c.execute(
                "INSERT INTO cards(id,labels) VALUES (?1,?2)",
                params![card["id"].as_str().unwrap(), card["labels"].to_string()],
            )
            .unwrap();
            for event in card["events"].as_array().unwrap() {
                c.execute("INSERT INTO events(seq,project,card,kind,at,payload) VALUES (?1,?2,?3,'card',?4,?5)", params![event["seq"].as_i64().unwrap(), event["project"].as_str().unwrap(), card["id"].as_str().unwrap(), event["at"].as_str().unwrap(), event["data"].to_string()]).unwrap();
            }
        }
        let mut count = 0;
        let mut raw_on_time = 0;
        let mut calibrated_on_time = 0;
        let mut raw_close = 0;
        let mut calibrated_close = 0;
        // Samples are ordered by completion. Only older completions train each prediction.
        for project in cards
            .iter()
            .flat_map(|c| c["events"].as_array().unwrap())
            .filter_map(|e| e["project"].as_str())
            .collect::<std::collections::BTreeSet<_>>()
        {
            let samples = work_samples(&c, project).unwrap();
            for sample in &samples {
                let history: Vec<_> = samples
                    .iter()
                    .filter(|s| s.end_seq < sample.start_seq)
                    .cloned()
                    .collect();
                let Some(prediction) =
                    calibrated_ms(&history, &sample.labels, Some(sample.guess_ms))
                else {
                    continue;
                };
                count += 1;
                raw_on_time += usize::from(sample.actual_ms <= sample.guess_ms);
                calibrated_on_time += usize::from(sample.actual_ms <= prediction);
                raw_close +=
                    usize::from((0.5..=2.0).contains(&(sample.actual_ms / sample.guess_ms)));
                calibrated_close +=
                    usize::from((0.5..=2.0).contains(&(sample.actual_ms / prediction)));
            }
        }
        println!(
            "backtest: eligible={count}, raw_on_time={raw_on_time}, calibrated_on_time={calibrated_on_time}, raw_half_to_double={raw_close}, calibrated_half_to_double={calibrated_close}"
        );
        assert!(count > 0);
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
