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

/// Most recent completed estimates, excluding jobs, superseded guesses,
/// clears, open work and invalid timestamps. The first subsequent ETA or
/// move closes an estimate, but only a move supplies a completed sample.
pub fn work_samples(conn: &Connection, project: &str) -> ApiResult<Vec<WorkSample>> {
    read_work_samples(conn, project, false)
}

/// One query for the dashboard, bounded by the latest 2000 project moves.
pub fn dashboard_work_samples(conn: &Connection, project: &str) -> ApiResult<Vec<WorkSample>> {
    read_work_samples(conn, project, true)
}

fn read_work_samples(
    conn: &Connection,
    project: &str,
    bounded: bool,
) -> ApiResult<Vec<WorkSample>> {
    let mut stmt = conn.prepare(
        "SELECT e.at, json_extract(e.payload, '$.eta_at'), x.at,
                coalesce(json_extract(e.payload, '$.labels'), c.labels, '[]'), e.seq, x.seq, e.card
         FROM events e INDEXED BY events_project JOIN events x ON x.seq = (
           SELECT min(n.seq) FROM events n INDEXED BY events_card WHERE n.card = e.card AND n.seq > e.seq
             AND n.kind = 'card' AND json_extract(n.payload, '$.change') IN ('eta','moved'))
         LEFT JOIN cards c ON c.id = e.card
         WHERE e.project = ?1 AND e.kind = 'card'
           AND e.seq >= CASE WHEN ?2 THEN coalesce((
             SELECT min(seq) FROM (
               SELECT seq FROM events INDEXED BY events_project
               WHERE project = ?1 AND kind = 'card' AND json_extract(payload, '$.change') = 'moved'
               ORDER BY seq DESC LIMIT 2000
             )
           ), 0) ELSE 0 END
           AND json_extract(e.payload, '$.change') = 'eta'
           AND coalesce(json_extract(e.payload, '$.column_role'), json_extract(e.payload, '$.column')) IN ('working', 'in_progress')
           AND coalesce(json_extract(e.payload, '$.job'), 0) = 0
           AND json_extract(x.payload, '$.change') = 'moved'
         ORDER BY x.seq DESC",
    )?;
    let rows = stmt.query_map(params![project, bounded], |r| {
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

/// Accuracy of raw worker guesses for the latest 30 distinct completed cards
/// in the bounded history. Null percentages mean there are no samples.
#[derive(serde::Serialize)]
pub struct WorkAccuracy {
    pub samples: usize,
    pub on_time_pct: Option<f64>,
    pub within_half_to_double_pct: Option<f64>,
}

#[derive(serde::Serialize)]
pub struct AccuracyView {
    pub raw: WorkAccuracy,
    pub calibrated: WorkAccuracy,
}

/// Backtest each recent card against only work completed before its guess.
/// Missing calibration history is excluded, not counted as a raw prediction.
pub fn accuracy_view(samples: &[WorkSample]) -> AccuracyView {
    let mut seen = std::collections::HashSet::new();
    let predictions: Vec<_> = samples
        .iter()
        .filter(|s| seen.insert(&s.card))
        .take(30)
        .filter_map(|s| {
            let older: Vec<_> = samples
                .iter()
                .filter(|h| h.end_seq < s.start_seq)
                .cloned()
                .collect();
            calibrated_ms(&older, &s.labels, Some(s.guess_ms)).map(|guess_ms| WorkSample {
                guess_ms,
                ..s.clone()
            })
        })
        .collect();
    AccuracyView {
        raw: work_accuracy(samples),
        calibrated: work_accuracy(&predictions),
    }
}

pub fn work_accuracy(samples: &[WorkSample]) -> WorkAccuracy {
    let mut seen = std::collections::HashSet::new();
    let recent: Vec<_> = samples
        .iter()
        .filter(|s| seen.insert(&s.card))
        .take(30)
        .collect();
    let percent =
        |count: usize| (!recent.is_empty()).then(|| count as f64 * 100.0 / recent.len() as f64);
    WorkAccuracy {
        samples: recent.len(),
        on_time_pct: percent(recent.iter().filter(|s| s.actual_ms <= s.guess_ms).count()),
        within_half_to_double_pct: percent(
            recent
                .iter()
                .filter(|s| s.actual_ms >= s.guess_ms / 2.0 && s.actual_ms <= s.guess_ms * 2.0)
                .count(),
        ),
    }
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
    let mut seen = std::collections::HashSet::new();
    let ratios = p70(samples
        .iter()
        .filter(|s| seen.insert(&s.card))
        .take(30)
        .map(|s| s.actual_ms / s.guess_ms)
        .collect());
    seen.clear();
    let actual = p70(samples
        .iter()
        .filter(|s| seen.insert(&s.card))
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
}

impl ReviewEstimates {
    pub fn worker_ms(
        &mut self,
        conn: &Connection,
        project: &str,
        labels: &[String],
        guess: Option<f64>,
    ) -> ApiResult<Option<f64>> {
        if !self.work.contains_key(project) {
            self.work
                .insert(project.to_owned(), work_samples(conn, project)?);
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

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE events(seq INTEGER PRIMARY KEY, project TEXT, card TEXT, kind TEXT, at TEXT, payload TEXT); CREATE INDEX events_project ON events(project, seq); CREATE INDEX events_card ON events(card, seq); CREATE TABLE cards(id TEXT, labels TEXT);").unwrap();
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
    fn accuracy_counts_inclusive_boundaries_and_latest_distinct_cards() {
        let empty = work_accuracy(&[]);
        assert_eq!(empty.samples, 0);
        assert_eq!(empty.on_time_pct, None);
        assert_eq!(empty.within_half_to_double_pct, None);
        let sample = |card: &str, actual_ms| WorkSample {
            card: card.into(),
            start_seq: 0,
            end_seq: 1,
            actual_ms,
            guess_ms: 100.0,
            labels: vec![],
        };
        let samples = vec![
            sample("a", 50.0),
            sample("b", 100.0),
            sample("c", 200.0),
            sample("d", 201.0),
            sample("a", 999.0),
        ];
        let accuracy = work_accuracy(&samples);
        assert_eq!(accuracy.samples, 4);
        assert_eq!(accuracy.on_time_pct, Some(50.0));
        assert_eq!(accuracy.within_half_to_double_pct, Some(75.0));
        let mut samples: Vec<_> = (0..30).map(|i| sample(&i.to_string(), 100.0)).collect();
        samples.push(sample("old", 999.0));
        assert_eq!(work_accuracy(&samples).samples, 30);
        assert_eq!(work_accuracy(&samples).on_time_pct, Some(100.0));
    }

    #[test]
    fn calibrated_accuracy_uses_only_completions_before_each_guess() {
        let samples: Vec<_> = (0..10)
            .rev()
            .map(|n| WorkSample {
                card: n.to_string(),
                start_seq: n * 2,
                end_seq: n * 2 + 1,
                actual_ms: 100.0,
                guess_ms: 1000.0,
                labels: vec!["type:impl".into()],
            })
            .collect();
        let view = accuracy_view(&samples);
        assert_eq!(view.raw.samples, 10);
        assert_eq!(view.raw.within_half_to_double_pct, Some(0.0));
        assert_eq!(view.calibrated.samples, 5);
        assert_eq!(view.calibrated.on_time_pct, Some(100.0));
        assert_eq!(view.calibrated.within_half_to_double_pct, Some(100.0));
        let overlapping: Vec<_> = samples
            .iter()
            .cloned()
            .map(|s| WorkSample { start_seq: 0, ..s })
            .collect();
        assert_eq!(accuracy_view(&overlapping).calibrated.samples, 0);
        assert_eq!(accuracy_view(&overlapping).calibrated.on_time_pct, None);
    }

    #[test]
    fn dashboard_history_is_move_bounded_without_shortening_worker_history() {
        let c = db();
        c.execute("INSERT INTO events(project,card,kind,at,payload) VALUES ('p','old','card','2026-11-08T00:00:00Z',?1)", [serde_json::json!({"change":"eta","column_role":"working","eta_at":"2026-11-08T00:10:00Z"}).to_string()]).unwrap();
        moved(&c, "p", "old", 5, "working", "review");
        assert_eq!(work_samples(&c, "p").unwrap().len(), 1);
        for _ in 0..2000 {
            c.execute("INSERT INTO events(project,card,kind,at,payload) VALUES ('p','noise','card','2026-11-08T00:06:00Z','{\"change\":\"comment\"}')", []).unwrap();
        }
        assert_eq!(dashboard_work_samples(&c, "p").unwrap().len(), 1);
        for _ in 0..2000 {
            moved(&c, "p", "noise", 6, "ready", "working");
        }
        assert!(dashboard_work_samples(&c, "p").unwrap().is_empty());
        assert_eq!(work_samples(&c, "p").unwrap().len(), 1);
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
