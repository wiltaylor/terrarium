//! Release gate using the last good schema before the ter-712 rollback.
//! Advance BASELINE only after a successful release, retaining this regression.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::{MIGRATIONS, NAMED_MIGRATIONS, migrate, migrate_named, migrate_with};
use rusqlite::Connection;

// Named migrations shipped in v0.55.0-alpha. Legacy migrations are frozen.
const BASELINE: &[&str] = &[
    "ter-121-thread-agent-ended.sql",
    "ter-124-github-issue-imports.sql",
    "ter-128-drop-old-versions.sql",
    "ter-149-thread-monitor.sql",
    "ter-157-card-move-history-index.sql",
    "ter-249-resource-reservations.sql",
    "ter-252-group-step-timing.sql",
    "ter-256-conflict-fixes.sql",
    "ter-260-agent-rpc-answers.sql",
    "ter-263-host-resume.sql",
    "ter-267-job-logs.sql",
    "ter-294-exedev-target-kind.sql",
    "ter-294-exedev-vms.sql",
    "ter-370-card-branch-head.sql",
    "ter-371-review-dispatcher.sql",
    "ter-394-usage-session-index.sql",
    "ter-401-job-stats-window.sql",
    "ter-450-day-start-schedules.sql",
    "ter-469-question-in-thread.sql",
    "ter-482-drop-name-watch.sql",
    "ter-553-accepted-heads.sql",
    "ter-555-agent-rpc-answers-age.sql",
    "ter-555b-agent-rpc-compaction.sql",
    "ter-580-backfilled-heads.sql",
    "ter-607-job-onto.sql",
    "ter-61-links.sql",
    "ter-614-card-review-entries.sql",
    "ter-62-remotes.sql",
    "ter-623-release-jobs.sql",
    "ter-623-release-targets.sql",
    "ter-630-cloud-subscriptions.sql",
    "ter-653-exedev-instance.sql",
    "ter-686-question-card-order.sql",
    "ter-687-request-threads.sql",
    "ter-687-zrequest-close-index.sql",
    "ter-688-default-inbox-index.sql",
    "ter-688-zter-334-inbox-pages.sql",
    "ter-689-home-moves.sql",
    "ter-689-home.sql",
    "ter-701-read-positions.sql",
    "ter-704-checkbox-questions.sql",
    "ter-719-job-e2e.sql",
    "ter-76-card-dispatcher.sql",
    "ter-78-target-roots.sql",
    "ter-78-target-roots.ter-498-base.sql",
    "ter-78-target-roots.ter-669-target-index.sql",
    "ter-79-card-trouble.sql",
    "ter-79-trouble-agent-index.sql",
    "ter-79-trouble-baseline.sql",
    "ter-81-research-helpers.sql",
    "ter-813-request-fallback.sql",
    "ter-825-legacy-log-listings.sql",
    "ter-858-artifact-pin.sql",
];

fn check_database(conn: &Connection) {
    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
    let violations: i64 = conn
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0, "foreign key violations");
}

fn seeded_previous_release() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    migrate_with(&mut conn, MIGRATIONS).unwrap();
    // Shipped named files have occasionally been edited. Use the actual
    // previous release SQL, not today's contents for the same filenames.
    conn.execute_batch(include_str!("baseline_v055.sql")).unwrap();
    let baseline: Vec<_> = BASELINE.iter().map(|name| (*name, "")).collect();
    migrate_named(&mut conn, &baseline).unwrap();
    // Disable FK enforcement only during the unordered synthetic inserts.
    // Check every FK before enabling production migration behavior.
    conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
    conn.execute_batch(include_str!("release_fixture.sql"))
        .unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    check_database(&conn);
    // Fail closed if any baseline application table was omitted.
    let mut tables = conn
        .prepare(
            "SELECT name FROM pragma_table_list WHERE schema = 'main' AND type = 'table'
         AND name NOT LIKE 'sqlite_%' AND name <> 'schema_migrations'",
        )
        .unwrap();
    for name in tables.query_map([], |r| r.get::<_, String>(0)).unwrap() {
        let name = name.unwrap();
        let count: i64 = conn
            .query_row(
                &format!("SELECT count(*) FROM \"{}\"", name.replace('"', "\"\"")),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(count > 0, "empty baseline table: {name}");
    }
    drop(tables);
    conn
}

#[test]
fn release_migrations_upgrade_populated_previous_release() {
    let mut conn = seeded_previous_release();
    let pending = NAMED_MIGRATIONS.len() - BASELINE.len();
    assert!(pending > 0, "baseline must exercise pending migrations");
    assert_eq!(migrate(&mut conn).unwrap(), pending);
    check_database(&conn);
    assert_eq!(migrate(&mut conn).unwrap(), 0);
    let waits: i64 = conn
        .query_row(
            "SELECT count(*) FROM card_waits WHERE card IN ('cards_id_1', 'fixture-card-3')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(waits, 2, "one wait per dependent card, not per dependency");
    let links: i64 = conn
        .query_row(
            "SELECT count(*) FROM thread_cards WHERE thread = 'threads_id_1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(links, 3, "migration lost thread links");
}

#[test]
fn fixture_rejects_unfixed_v057_card_wait_migration() {
    let mut conn = seeded_previous_release();
    let error = migrate_named(
        &mut conn,
        &[(
            "ter-712-card-waits.sql",
            include_str!("unfixed_v057_card_waits.sql"),
        )],
    )
    .unwrap_err();
    let cause = format!("{error:#}");
    assert!(
        cause.contains("UNIQUE constraint failed: card_waits.card"),
        "{cause}"
    );
    check_database(&conn);
    assert!(
        !conn.table_exists(None, "card_waits").unwrap(),
        "failed migration must roll back"
    );
}
