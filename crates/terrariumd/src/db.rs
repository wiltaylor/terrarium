//! SQLite storage and schema migrations.
//!
//! Each migration runs once, in order. `PRAGMA user_version` records how many
//! have run. Never edit a migration that has shipped; add a new one.

use std::path::Path;

use anyhow::Context;
use rusqlite::Connection;

/// The migrations, in order. Index + 1 is the schema version.
const MIGRATIONS: &[&str] = &[
    // 1: core tables. Board, memory, wiki, artifact, version, and usage
    // tables (PLAN §12-§16) come in later migrations.
    r#"
    CREATE TABLE projects (
        name        TEXT PRIMARY KEY,
        description TEXT,
        created_at  TEXT NOT NULL,
        updated_at  TEXT NOT NULL
    );

    -- `project` has no foreign key: projects come from definition files and
    -- are synced into the table, so an agent may load before its project row.
    CREATE TABLE agents (
        id           TEXT PRIMARY KEY,          -- ULID
        name         TEXT NOT NULL,
        project      TEXT NOT NULL,
        harness      TEXT NOT NULL CHECK (harness IN ('claude', 'codex', 'copilot', 'pi')),
        subscription TEXT,
        model        TEXT,
        prompt       TEXT,
        roles        TEXT NOT NULL DEFAULT '[]', -- JSON array of roles
        policy       TEXT NOT NULL DEFAULT '{}', -- JSON policy
        restart      TEXT NOT NULL DEFAULT 'on-failure'
                     CHECK (restart IN ('always', 'on-failure', 'never')),
        status       TEXT NOT NULL DEFAULT 'stopped'
                     CHECK (status IN ('starting', 'idle', 'working', 'blocked',
                                       'error', 'stopped', 'restarting')),
        workspace    TEXT NOT NULL,
        created_at   TEXT NOT NULL,
        updated_at   TEXT NOT NULL,
        UNIQUE (project, name)
    );

    CREATE TABLE sessions (
        id                 TEXT PRIMARY KEY,    -- ULID
        agent_id           TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
        harness_session_id TEXT,                -- the harness's own id, once known
        current            INTEGER NOT NULL DEFAULT 0,
        started_at         TEXT NOT NULL,
        ended_at           TEXT,
        end_reason         TEXT
    );
    CREATE INDEX sessions_agent ON sessions(agent_id, started_at);

    CREATE TABLE messages (
        id           TEXT PRIMARY KEY,          -- ULID
        thread_id    TEXT NOT NULL,
        from_agent   TEXT NOT NULL,             -- agent id, `operator`, or `project:<name>`
        to_agent     TEXT NOT NULL,             -- agent id or `project:<name>`
        subject      TEXT,
        body         TEXT NOT NULL,
        state        TEXT NOT NULL DEFAULT 'queued'
                     CHECK (state IN ('queued', 'delivered', 'read', 'replied')),
        created_at   TEXT NOT NULL,
        delivered_at TEXT,
        read_at      TEXT
    );
    CREATE INDEX messages_to ON messages(to_agent, state);
    CREATE INDEX messages_thread ON messages(thread_id, created_at);

    CREATE TABLE events (
        id         TEXT PRIMARY KEY,            -- ULID
        agent_id   TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
        session_id TEXT REFERENCES sessions(id) ON DELETE SET NULL,
        at         TEXT NOT NULL,
        type       TEXT NOT NULL,
        payload    TEXT NOT NULL                -- JSON of the event
    );
    CREATE INDEX events_agent ON events(agent_id, at);
    "#,
];

/// Open the database, set pragmas, and run pending migrations.
pub fn open(path: &Path) -> anyhow::Result<Connection> {
    let mut conn =
        Connection::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    migrate(&mut conn)?;
    Ok(conn)
}

/// Run every migration newer than the database's `user_version`.
pub fn migrate(conn: &mut Connection) -> anyhow::Result<usize> {
    let current: usize = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let mut applied = 0;
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(current) {
        let version = index + 1;
        let tx = conn.transaction()?;
        tx.execute_batch(sql)
            .with_context(|| format!("migration {version} failed"))?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
        tracing::info!(version, "applied migration");
        applied += 1;
    }
    Ok(applied)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn migrations_create_tables_and_are_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());
        assert_eq!(migrate(&mut conn).unwrap(), 0);
        for table in ["projects", "agents", "sessions", "messages", "events"] {
            let n: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "table {table}");
        }
    }

    #[test]
    fn open_creates_a_file_database() {
        let dir = std::env::temp_dir().join(format!("terrariumd-db-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("terrarium.db");
        drop(open(&path).unwrap());
        let conn = open(&path).unwrap();
        let v: usize = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, MIGRATIONS.len());
        drop(conn);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
