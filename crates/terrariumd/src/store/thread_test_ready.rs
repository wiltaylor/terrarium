//! Installed-card gate for thread test requests. All reads start at one thread's links.
use rusqlite::{Connection, params};

use crate::{
    db::Tx,
    error::{ApiError, ApiResult},
};

/// Return the linked-card fingerprint and reasons the work cannot be tested yet.
pub fn readiness(
    conn: &Connection,
    thread: &str,
    running: &str,
) -> ApiResult<(String, Vec<String>)> {
    let running = semver::Version::parse(running)
        .map_err(|_| ApiError::invalid("invalid running daemon version"))?;
    let mut st = conn.prepare(
        "SELECT c.id, k.role, c.labels FROM thread_cards tc
         JOIN cards c ON c.id = tc.card JOIN board_columns k ON k.id = c.column_id
         WHERE tc.thread = ?1 ORDER BY c.id",
    )?;
    let rows = st.query_map([thread], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut cards = Vec::new();
    let mut reasons = Vec::new();
    for row in rows {
        let (card, role, labels) = row?;
        cards.push(card.clone());
        if role.as_deref() != Some("done") {
            reasons.push(format!("{card}: not done"));
        }
        let labels: Vec<String> =
            serde_json::from_str(&labels).map_err(|_| ApiError::invalid("invalid card labels"))?;
        for label in labels.iter().filter_map(|s| s.strip_prefix("version:")) {
            // Version groups use X.Y.Z for their X.Y.Z-alpha pre-release.
            let version = if label.contains('-') {
                label.to_owned()
            } else {
                format!("{label}-alpha")
            };
            if !semver::Version::parse(&version).is_ok_and(|v| v <= running) {
                reasons.push(format!(
                    "{card}: version {label} not installed (daemon {running})"
                ));
            }
        }
    }
    let monitor: Option<String> =
        conn.query_row("SELECT monitor FROM threads WHERE id = ?1", [thread], |r| {
            r.get(0)
        })?;
    if cards.is_empty() && monitor.is_none() {
        reasons.push("no linked cards".to_owned());
    }
    Ok((
        serde_json::to_string(&cards).map_err(|_| ApiError::invalid("invalid card set"))?,
        reasons,
    ))
}

pub fn check(conn: &Connection, thread: &str) -> ApiResult<()> {
    let (_, reasons) = readiness(conn, thread, terrarium_version::VERSION)?;
    if !reasons.is_empty() {
        return Err(ApiError::conflict(format!(
            "thread not ready to test: {}",
            reasons.join(", ")
        )));
    }
    Ok(())
}

/// Persist once per linked-card set, in the same transaction as its wake.
pub fn claim(t: &mut Tx<'_>, thread: &str) -> ApiResult<bool> {
    let (cards, reasons) = readiness(t, thread, terrarium_version::VERSION)?;
    if !reasons.is_empty() {
        return Ok(false);
    }
    Ok(t.execute("UPDATE threads SET test_ready_cards = ?2 WHERE id = ?1 AND (test_ready_cards IS NOT ?2 OR (monitor IS NOT NULL AND monitor_done = 0))", params![thread, cards])? > 0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn readiness_compares_release_labels_with_the_running_prerelease() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE threads(id TEXT PRIMARY KEY, monitor TEXT);
             CREATE TABLE thread_cards(thread TEXT, card TEXT, PRIMARY KEY(thread, card));
             CREATE TABLE cards(id TEXT PRIMARY KEY, column_id TEXT, labels TEXT);
             CREATE TABLE board_columns(id TEXT PRIMARY KEY, role TEXT);
             INSERT INTO threads VALUES ('t', NULL);
             INSERT INTO board_columns VALUES ('d', 'done'), ('r', 'review');
             INSERT INTO cards VALUES ('A', 'd', '[\"version:1.2.3\"]'), ('B', 'r', '[]');
             INSERT INTO thread_cards VALUES ('t', 'A'), ('t', 'B');",
        )
        .unwrap();
        assert_eq!(
            readiness(&c, "t", "1.2.3-alpha").unwrap().1,
            ["B: not done"]
        );
        c.execute("UPDATE cards SET column_id = 'd'", []).unwrap();
        assert!(readiness(&c, "t", "1.2.3-alpha").unwrap().1.is_empty());
        assert!(readiness(&c, "t", "1.2.3").unwrap().1.is_empty());
        assert!(readiness(&c, "t", "1.2.2").unwrap().1[0].contains("not installed"));
        c.execute(
            "UPDATE cards SET labels = '[\"version:1.2.3-beta\"]' WHERE id = 'A'",
            [],
        )
        .unwrap();
        assert!(readiness(&c, "t", "1.2.3-alpha").unwrap().1[0].contains("not installed"));
        c.execute(
            "UPDATE cards SET labels = '[\"version:vnext\"]' WHERE id = 'A'",
            [],
        )
        .unwrap();
        assert!(readiness(&c, "t", "1.2.3").unwrap().1[0].contains("not installed"));
        c.execute("UPDATE cards SET labels = '[]'", []).unwrap();
        assert!(readiness(&c, "t", "0.0.0-alpha").unwrap().1.is_empty());
    }
}
