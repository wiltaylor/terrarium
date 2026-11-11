//! SQLite work must grow with the board, not with its square.
#![allow(clippy::unwrap_used)]

use crate::common::TestDaemon;
use rusqlite::params;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use terrariumd::store::{Page, board};

#[tokio::test]
async fn board_summary_sqlite_work_grows_near_linearly() {
    let mut measured = Vec::new();
    for count in [294, 2940] {
        let td = TestDaemon::start().await;
        td.post(
            "/api/projects",
            json!({"name": "proj", "card_prefix": "TER"}),
        )
        .await
        .ok();
        let mut conn = terrariumd::db::open(&td.state.join("terrarium.db")).unwrap();
        let tx = conn.transaction().unwrap();
        for n in 1..=count {
            let id = format!("TER-{n}");
            tx.execute("INSERT INTO cards(id, project, number, column_id, position, title, body, created_by, created_at, updated_at)
                VALUES (?1, 'proj', ?2, (SELECT id FROM board_columns WHERE project = 'proj' AND name = ?3), ?2, 'Fixture', '', 'operator', '2026-11-08T00:00:00Z', '2026-11-08T00:00:00Z')",
                params![id, n, if n % 3 == 0 { "ready" } else { "done" }]).unwrap();
            for j in 0..10 {
                tx.execute("INSERT INTO card_comments(id, card, author, body, created_at) VALUES (?1, ?2, 'operator', 'comment', '2026-11-08T00:00:00Z')",
                    params![format!("comment-{n}-{j}"), id]).unwrap();
            }
        }
        tx.commit().unwrap();
        // Exercise both empty dependency lists and populated lists. Done
        // dependencies remain in depends_on but must not appear in blocked_by.
        conn.execute(
            "INSERT INTO card_deps(card, depends_on) VALUES ('TER-3', 'TER-1'), ('TER-3', 'TER-6')",
            [],
        )
        .unwrap();
        let steps = Arc::new(AtomicU64::new(0));
        let counter = steps.clone();
        conn.progress_handler(
            1000,
            Some(move || {
                counter.fetch_add(1000, Ordering::Relaxed);
                false
            }),
        );
        let result = board::board_summary(&conn, "proj", false, &Page::default());
        conn.progress_handler(0, None::<fn() -> bool>);
        let summary = result.unwrap();
        let cards = summary["cards"].as_array().unwrap();
        assert_eq!(cards.len(), count / 3);
        let card = cards.iter().find(|c| c["id"] == "TER-3").unwrap();
        assert_eq!(card["blocked_by"], json!(["TER-6"]));
        assert_eq!(card["depends_on"], json!(["TER-1", "TER-6"]));
        assert_eq!(card["comment_count"], 10);
        println!(
            "board count={count}: {} VM instructions",
            steps.load(Ordering::Relaxed)
        );
        measured.push(steps.load(Ordering::Relaxed));
    }
    assert!(measured[0] > 0);
    // 10x the data gets 15x the work, allowing sorting and fixed overhead,
    // but rejecting the former roughly 92x growth. No wall-clock budget.
    assert!(measured[1] <= measured[0] * 15, "VM growth: {measured:?}");
}
