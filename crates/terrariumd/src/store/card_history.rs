//! Estimates from card move events. Review uses the last 20 completed
//! visits in this project, with at least three samples. Repeated visits
//! count separately. Only the latest 2000 project card moves are considered, bounding
//! query work. Open visits and malformed or backwards times do not count.

use chrono::{DateTime, Duration};
use rusqlite::{Connection, params};

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
        Some(guess) if guess.is_finite() && guess > 0.0 => Some((guess * ratios? * actual?).sqrt()),
        Some(_) => None,
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
    single_card: Option<String>,
    review: std::collections::HashMap<String, Option<Duration>>,
    work: std::collections::HashMap<String, Vec<WorkSample>>,
    entries: std::collections::HashMap<String, std::collections::HashMap<String, (String, String)>>,
}

// The inner card-index lookup stops at each board card's latest move. The
// outer lookup uses the guarded partial move index, not all project history.
const COLUMN_ENTRIES_SQL: &str = "
    SELECT e.card, e.at,
      coalesce(json_extract(e.payload, '$.to_role'), json_extract(e.payload, '$.to'), '')
    FROM cards c JOIN events e INDEXED BY events_card_moves ON e.seq = (
      SELECT seq FROM events WHERE card = c.id AND kind = 'card'
        AND CASE WHEN json_valid(payload) THEN json_extract(payload, '$.change') END = 'moved'
      ORDER BY seq DESC LIMIT 1
    )
    WHERE c.project = ?1 AND c.archived = 0 AND e.project = ?1 AND e.kind = 'card'
      AND CASE WHEN json_valid(e.payload) THEN json_extract(e.payload, '$.change') END = 'moved'";

impl ReviewEstimates {
    pub fn for_card(card: &str) -> Self {
        Self {
            single_card: Some(card.to_owned()),
            ..Default::default()
        }
    }

    /// One latest-column-entry query per project per response, including misses.
    pub fn entered_at(
        &mut self,
        conn: &Connection,
        project: &str,
        card: &str,
        role: Option<&str>,
    ) -> ApiResult<Option<String>> {
        if !self.entries.contains_key(project) {
            let sql = if self.single_card.is_some() {
                format!("{COLUMN_ENTRIES_SQL} AND c.id = ?2")
            } else {
                COLUMN_ENTRIES_SQL.to_owned()
            };
            let mut stmt = conn.prepare(&sql)?;
            let mut parameters = vec![project];
            if let Some(card) = self.single_card.as_deref() {
                parameters.push(card);
            }
            let rows = stmt.query_map(rusqlite::params_from_iter(parameters), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            let mut entries = std::collections::HashMap::new();
            for row in rows {
                let (card, at, column) = row?;
                entries.entry(card).or_insert((at, column));
            }
            self.entries.insert(project.to_owned(), entries);
        }
        Ok(self
            .entries
            .get(project)
            .and_then(|e| e.get(card))
            .filter(|(_, column)| role.is_none_or(|r| r == column))
            .map(|(at, _)| at.clone()))
    }

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
        let start = self.entered_at(conn, project, card, Some(super::board::REVIEW))?;
        Ok(start.and_then(|start| {
            let at = DateTime::parse_from_rfc3339(&start)
                .ok()?
                .checked_add_signed(median)?;
            Some((at.to_rfc3339(), start))
        }))
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use serde_json::json;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE events(seq INTEGER PRIMARY KEY, project TEXT, card TEXT, kind TEXT, at TEXT, payload TEXT); CREATE TABLE cards(id TEXT PRIMARY KEY, labels TEXT, project TEXT, archived INTEGER DEFAULT 0); CREATE INDEX events_card ON events(card, seq);").unwrap();
        c.execute_batch(include_str!(
            "../../migrations/ter-157-card-move-history-index.sql"
        ))
        .unwrap();
        c.execute_batch("CREATE INDEX events_project ON events(project, seq);")
            .unwrap();
        c
    }

    fn moved(c: &Connection, project: &str, card: &str, minute: u32, from: &str, to: &str) {
        c.execute(
            "INSERT OR IGNORE INTO cards(id,project) VALUES (?1,?2)",
            params![card, project],
        )
        .unwrap();
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
        for guess in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(calibrated_ms(&samples, &labels, Some(guess)), None);
        }
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
    fn accuracy_history_ignores_non_json_and_non_move_events() {
        let c = db();
        c.execute("INSERT INTO events(project,card,kind,at,payload) VALUES ('p','old','card','2026-11-08T00:00:00Z',?1)", [serde_json::json!({"change":"eta","column_role":"working","eta_at":"2026-11-08T00:10:00Z"}).to_string()]).unwrap();
        moved(&c, "p", "old", 5, "working", "review");
        assert_eq!(work_samples(&c, "p").unwrap().len(), 1);
        for _ in 0..2000 {
            c.execute("INSERT INTO events(project,card,kind,at,payload) VALUES ('p','noise','card','2026-11-08T00:06:00Z','{\"change\":\"comment\"}')", []).unwrap();
        }
        c.execute("INSERT INTO events(project,card,kind,at,payload) VALUES ('p','old','card','2026-11-08T00:06:00Z','not JSON')", []).unwrap();
        let samples = work_samples(&c, "p").unwrap();
        assert_eq!(accuracy_view(&samples).raw.samples, 1);
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
        let mut cache = ReviewEstimates::default();
        // Five distinct completions make the cached answer nonempty.
        for n in 0..5 {
            let card = format!("cache{n}");
            c.execute("INSERT INTO events(project,card,kind,at,payload) VALUES ('p',?1,'card','2026-11-08T00:00:00Z',?2)", params![card, json!({"change":"eta","column_role":"working","eta_at":"2026-11-08T00:10:00Z","labels":["type:impl"]}).to_string()]).unwrap();
            moved(&c, "p", &card, 5, "working", "review");
        }
        let labels = vec!["type:impl".into()];
        let first = cache.worker_ms(&c, "p", &labels, None).unwrap();
        assert!(first.is_some());
        c.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<2000) INSERT INTO events(project,card,kind,payload) SELECT 'p','noise','card','{\"change\":\"moved\"}' FROM n;").unwrap();
        assert!(work_samples(&c, "p").unwrap().is_empty());
        assert_eq!(cache.worker_ms(&c, "p", &labels, None).unwrap(), first);
        assert_eq!(
            ReviewEstimates::default()
                .worker_ms(&c, "p", &labels, None)
                .unwrap(),
            None
        );
    }

    #[test]
    fn single_card_render_only_loads_that_cards_entry() {
        let c = db();
        moved(&c, "p", "a", 1, "ready", "working");
        moved(&c, "p", "b", 2, "working", "review");
        let mut cache = ReviewEstimates::for_card("a");
        assert!(cache.entered_at(&c, "p", "a", None).unwrap().is_some());
        assert_eq!(cache.entries["p"].len(), 1);
        assert!(!cache.entries["p"].contains_key("b"));
        let mut stmt = c
            .prepare(&format!(
                "EXPLAIN QUERY PLAN {COLUMN_ENTRIES_SQL} AND c.id = ?2"
            ))
            .unwrap();
        let plan = stmt
            .query_map(["p", "a"], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("\n");
        assert!(plan.contains("sqlite_autoindex_cards_1 (id=?"), "{plan}");
        assert!(!plan.contains("SCAN c"), "{plan}");
    }

    #[test]
    fn column_entries_are_cached_for_all_cards_including_misses() {
        let c = db();
        moved(&c, "p", "a", 1, "ready", "working");
        moved(&c, "p", "b", 2, "working", "review");
        c.execute(
            "INSERT INTO events(project,card,kind,payload) VALUES ('p','a','card','broken JSON')",
            [],
        )
        .unwrap();
        assert!(work_samples(&c, "p").unwrap().is_empty());
        let mut stmt = c
            .prepare(&format!("EXPLAIN QUERY PLAN {COLUMN_ENTRIES_SQL}"))
            .unwrap();
        let plan = stmt
            .query_map(["p"], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("\n");
        assert!(plan.contains("events_card_moves"), "{plan}");
        assert!(plan.contains("events_card (card=?"), "{plan}");
        assert!(!plan.contains("SCAN e"), "{plan}");
        let mut cache = ReviewEstimates::default();
        assert!(cache.entered_at(&c, "p", "a", None).unwrap().is_some());
        c.execute("DELETE FROM events", []).unwrap();
        assert_eq!(
            cache
                .entered_at(&c, "p", "b", Some("review"))
                .unwrap()
                .as_deref(),
            Some("2026-11-08T00:02:00Z")
        );
        moved(&c, "p", "missing", 3, "ready", "working");
        assert!(
            cache
                .entered_at(&c, "p", "missing", None)
                .unwrap()
                .is_none()
        );
        assert!(cache.entered_at(&c, "other", "a", None).unwrap().is_none());
    }

    /// TER-394: worker samples come from the cards of the latest 2000
    /// project moves, through the move index, with no per-event subquery,
    /// and malformed payloads do not fail the read.
    #[test]
    fn worker_samples_are_bounded_by_recent_moves_and_use_the_index() {
        let c = db();
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
