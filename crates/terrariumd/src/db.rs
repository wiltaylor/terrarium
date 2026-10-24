//! SQLite storage and schema migrations.
//!
//! Each migration runs once, in order. `PRAGMA user_version` records how many
//! have run. Never edit a migration that has shipped; add a new one.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use rusqlite::Connection;

use crate::error::{ApiError, ApiResult};
use crate::events::{EventDraft, EventHub, StoredEvent};

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
    // 2: the store for the API (PLAN §5, §10, §12, §13, §17).
    r#"
    -- Projects: card prefix and counter, and where the definition lives.
    ALTER TABLE projects ADD COLUMN prefix TEXT NOT NULL DEFAULT '';
    ALTER TABLE projects ADD COLUMN card_seq INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE projects ADD COLUMN source TEXT NOT NULL DEFAULT 'api'
        CHECK (source IN ('file', 'api'));
    CREATE UNIQUE INDEX projects_prefix ON projects(prefix) WHERE prefix <> '';

    -- Agents: where the definition lives. `agents.roles` is not used any
    -- more; the `roles` table holds the roles.
    ALTER TABLE agents ADD COLUMN source TEXT NOT NULL DEFAULT 'api'
        CHECK (source IN ('file', 'api'));

    CREATE TABLE repos (
        id             TEXT PRIMARY KEY,        -- ULID
        project        TEXT NOT NULL REFERENCES projects(name) ON DELETE CASCADE,
        name           TEXT NOT NULL,
        url            TEXT NOT NULL,
        default_branch TEXT,
        credential     TEXT,                    -- key name in /state/credentials
        path           TEXT,
        source         TEXT NOT NULL DEFAULT 'api' CHECK (source IN ('file', 'api')),
        created_at     TEXT NOT NULL,
        UNIQUE (project, name)
    );

    -- Roles above `member`. `project` is the scope of `project_admin`;
    -- it is NULL for `sysadmin`.
    CREATE TABLE roles (
        agent_id   TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
        role       TEXT NOT NULL CHECK (role IN ('project_admin', 'sysadmin')),
        project    TEXT,
        granted_by TEXT NOT NULL,
        granted_at TEXT NOT NULL,
        PRIMARY KEY (agent_id, role)
    );

    -- Events become append-only: no foreign key, so deleting an agent keeps
    -- its history. `seq` orders the log and is the `?since=` cursor.
    DROP INDEX events_agent;
    CREATE TABLE events_v2 (
        seq        INTEGER PRIMARY KEY AUTOINCREMENT,
        id         TEXT NOT NULL UNIQUE,        -- ULID
        agent_id   TEXT,
        agent_name TEXT,                        -- kept, so the log reads after a delete
        project    TEXT,
        card       TEXT,
        session_id TEXT,
        actor      TEXT,
        kind       TEXT NOT NULL,
        summary    TEXT NOT NULL DEFAULT '',
        payload    TEXT NOT NULL,               -- JSON
        at         TEXT NOT NULL
    );
    INSERT INTO events_v2 (id, agent_id, session_id, kind, payload, at)
        SELECT id, agent_id, session_id, type, payload, at FROM events ORDER BY at;
    DROP TABLE events;
    ALTER TABLE events_v2 RENAME TO events;
    CREATE INDEX events_agent ON events(agent_id, seq);
    CREATE INDEX events_project ON events(project, seq);
    CREATE INDEX events_card ON events(card, seq);

    -- Messages: the kind of each address, and the message replied to.
    ALTER TABLE messages ADD COLUMN from_kind TEXT NOT NULL DEFAULT 'agent'
        CHECK (from_kind IN ('agent', 'project', 'operator'));
    ALTER TABLE messages ADD COLUMN to_kind TEXT NOT NULL DEFAULT 'agent'
        CHECK (to_kind IN ('agent', 'project', 'operator'));
    ALTER TABLE messages ADD COLUMN in_reply_to TEXT;
    CREATE INDEX messages_from ON messages(from_agent);

    CREATE TABLE audit (
        seq     INTEGER PRIMARY KEY AUTOINCREMENT,
        id      TEXT NOT NULL UNIQUE,           -- ULID
        actor   TEXT NOT NULL,                  -- `operator`, `agent:<ulid>`, `system`
        action  TEXT NOT NULL,                  -- for example `agent.start`
        target  TEXT NOT NULL,
        project TEXT,
        detail  TEXT NOT NULL DEFAULT '{}',     -- JSON
        at      TEXT NOT NULL
    );
    CREATE INDEX audit_actor ON audit(actor, seq);

    -- The kanban board (PLAN §13). One board per project.
    CREATE TABLE boards (
        project    TEXT PRIMARY KEY REFERENCES projects(name) ON DELETE CASCADE,
        created_at TEXT NOT NULL
    );
    CREATE TABLE board_columns (
        id        INTEGER PRIMARY KEY,
        project   TEXT NOT NULL REFERENCES boards(project) ON DELETE CASCADE,
        name      TEXT NOT NULL,
        position  INTEGER NOT NULL,
        wip_limit INTEGER,
        UNIQUE (project, name)
    );
    CREATE TABLE cards (
        id         TEXT PRIMARY KEY,            -- `<prefix>-<number>`, for example TER-42
        project    TEXT NOT NULL REFERENCES boards(project) ON DELETE CASCADE,
        number     INTEGER NOT NULL,
        column_id  INTEGER NOT NULL REFERENCES board_columns(id),
        position   INTEGER NOT NULL,
        title      TEXT NOT NULL,
        body       TEXT NOT NULL DEFAULT '',
        assignee   TEXT,                        -- agent id or `operator`
        labels     TEXT NOT NULL DEFAULT '[]',  -- JSON array
        repo       TEXT,
        branch     TEXT,
        created_by TEXT NOT NULL,               -- agent id or `operator`
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        archived   INTEGER NOT NULL DEFAULT 0,
        UNIQUE (project, number)
    );
    CREATE INDEX cards_column ON cards(column_id, position);
    CREATE TABLE card_deps (
        card       TEXT NOT NULL REFERENCES cards(id) ON DELETE CASCADE,
        depends_on TEXT NOT NULL REFERENCES cards(id) ON DELETE CASCADE,
        PRIMARY KEY (card, depends_on)
    );
    CREATE TABLE card_comments (
        id         TEXT PRIMARY KEY,            -- ULID
        card       TEXT NOT NULL REFERENCES cards(id) ON DELETE CASCADE,
        author     TEXT NOT NULL,               -- agent id, `operator`, or `project:<name>`
        body       TEXT NOT NULL,
        created_at TEXT NOT NULL
    );
    CREATE INDEX card_comments_card ON card_comments(card, created_at);
    CREATE TABLE card_links (
        card       TEXT NOT NULL REFERENCES cards(id) ON DELETE CASCADE,
        kind       TEXT NOT NULL CHECK (kind IN ('thread', 'branch', 'card', 'url')),
        ref        TEXT NOT NULL,
        created_by TEXT NOT NULL,
        created_at TEXT NOT NULL,
        PRIMARY KEY (card, kind, ref)
    );
    CREATE INDEX card_links_ref ON card_links(kind, ref);
    "#,
    // 3: memory graph, wiki, artifacts, versions (PLAN §14, §15).
    r#"
    -- Project-local counters for short IDs (`MEM-17`, `ART-9`).
    CREATE TABLE object_seq (
        project TEXT NOT NULL REFERENCES projects(name) ON DELETE CASCADE,
        kind    TEXT NOT NULL,                  -- `memory`, `artifact`
        last    INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (project, kind)
    );

    -- The memory graph (PLAN §14).
    CREATE TABLE memories (
        pk            INTEGER PRIMARY KEY,
        project       TEXT NOT NULL REFERENCES projects(name) ON DELETE CASCADE,
        num           INTEGER NOT NULL,         -- `MEM-<num>`
        title         TEXT NOT NULL,
        body          TEXT NOT NULL DEFAULT '',
        tags          TEXT NOT NULL DEFAULT '[]', -- JSON array
        author        TEXT NOT NULL,            -- agent id or `operator`
        created_at    TEXT NOT NULL,
        updated_at    TEXT NOT NULL,
        superseded_by INTEGER REFERENCES memories(pk) ON DELETE SET NULL,
        UNIQUE (project, num)
    );
    CREATE TABLE memory_sources (
        memory     INTEGER NOT NULL REFERENCES memories(pk) ON DELETE CASCADE,
        kind       TEXT NOT NULL CHECK (kind IN ('card', 'thread', 'path', 'commit', 'url')),
        ref        TEXT NOT NULL,
        PRIMARY KEY (memory, kind, ref)
    );
    CREATE TABLE memory_links (
        from_id    INTEGER NOT NULL REFERENCES memories(pk) ON DELETE CASCADE,
        to_id      INTEGER NOT NULL REFERENCES memories(pk) ON DELETE CASCADE,
        relation   TEXT NOT NULL CHECK (relation IN ('relates_to', 'supports', 'contradicts',
                       'supersedes', 'derived_from', 'part_of', 'example_of')),
        author     TEXT NOT NULL,
        note       TEXT,
        created_at TEXT NOT NULL,
        PRIMARY KEY (from_id, to_id, relation)
    );
    CREATE INDEX memory_links_to ON memory_links(to_id);
    CREATE VIRTUAL TABLE memories_fts USING fts5(
        title, body, tags, content='memories', content_rowid='pk'
    );
    CREATE TRIGGER memories_ai AFTER INSERT ON memories BEGIN
        INSERT INTO memories_fts (rowid, title, body, tags)
        VALUES (new.pk, new.title, new.body, new.tags);
    END;
    CREATE TRIGGER memories_ad AFTER DELETE ON memories BEGIN
        INSERT INTO memories_fts (memories_fts, rowid, title, body, tags)
        VALUES ('delete', old.pk, old.title, old.body, old.tags);
    END;
    CREATE TRIGGER memories_au AFTER UPDATE OF title, body, tags ON memories BEGIN
        INSERT INTO memories_fts (memories_fts, rowid, title, body, tags)
        VALUES ('delete', old.pk, old.title, old.body, old.tags);
        INSERT INTO memories_fts (rowid, title, body, tags)
        VALUES (new.pk, new.title, new.body, new.tags);
    END;

    -- The wiki (PLAN §15.1). A path is unique among pages that are not deleted.
    CREATE TABLE wiki_pages (
        id          TEXT PRIMARY KEY,           -- ULID
        project     TEXT NOT NULL REFERENCES projects(name) ON DELETE CASCADE,
        path        TEXT NOT NULL,
        title       TEXT NOT NULL,
        current_rev INTEGER NOT NULL,
        status      TEXT CHECK (status IN ('draft', 'ready_for_review', 'reviewed')),
        reviewer    TEXT,
        created_by  TEXT NOT NULL,
        created_at  TEXT NOT NULL,
        updated_at  TEXT NOT NULL,
        deleted_at  TEXT
    );
    CREATE UNIQUE INDEX wiki_pages_path ON wiki_pages(project, path) WHERE deleted_at IS NULL;
    CREATE TABLE wiki_revisions (
        page       TEXT NOT NULL REFERENCES wiki_pages(id) ON DELETE CASCADE,
        rev        INTEGER NOT NULL,
        author     TEXT NOT NULL,
        summary    TEXT NOT NULL,
        body       TEXT NOT NULL,
        created_at TEXT NOT NULL,
        PRIMARY KEY (page, rev)
    );
    -- The links of the current revision of each page. `target` is
    -- `project/path` for a page, the card ID, `project/MEM-n`,
    -- `project/ART-n`, or `project/<version name>`.
    CREATE TABLE wiki_links (
        page   TEXT NOT NULL REFERENCES wiki_pages(id) ON DELETE CASCADE,
        kind   TEXT NOT NULL CHECK (kind IN ('page', 'card', 'memory', 'version', 'artifact')),
        target TEXT NOT NULL,
        PRIMARY KEY (page, kind, target)
    );
    CREATE INDEX wiki_links_target ON wiki_links(kind, target);
    -- Full text over the current revision. `page` is the page ID.
    CREATE VIRTUAL TABLE wiki_fts USING fts5(page UNINDEXED, path, title, body);
    CREATE TRIGGER wiki_pages_ad AFTER DELETE ON wiki_pages BEGIN
        DELETE FROM wiki_fts WHERE page = old.id;
    END;

    -- Artifacts (PLAN §15.2). Blobs live in <state>/artifacts/<sha256>.
    CREATE TABLE artifacts (
        pk           INTEGER PRIMARY KEY,
        id           TEXT NOT NULL UNIQUE,      -- ULID; the serving URL uses it
        project      TEXT NOT NULL REFERENCES projects(name) ON DELETE CASCADE,
        num          INTEGER NOT NULL,          -- `ART-<num>`
        name         TEXT NOT NULL,
        content_type TEXT NOT NULL,
        size         INTEGER NOT NULL,
        sha256       TEXT NOT NULL,
        kind         TEXT NOT NULL CHECK (kind IN ('file', 'bundle')),
        created_by   TEXT NOT NULL,
        created_at   TEXT NOT NULL,
        warned_at    TEXT,                      -- the prune warning
        UNIQUE (project, num)
    );
    CREATE INDEX artifacts_sha ON artifacts(sha256);
    CREATE TABLE artifact_links (
        artifact   INTEGER NOT NULL REFERENCES artifacts(pk) ON DELETE CASCADE,
        kind       TEXT NOT NULL CHECK (kind IN ('page', 'card')),
        ref        TEXT NOT NULL,               -- page ID or card ID
        created_by TEXT NOT NULL,
        created_at TEXT NOT NULL,
        PRIMARY KEY (artifact, kind, ref)
    );
    CREATE INDEX artifact_links_ref ON artifact_links(kind, ref);

    -- Versions (PLAN §15.3).
    CREATE TABLE versions (
        id           TEXT PRIMARY KEY,          -- ULID
        project      TEXT NOT NULL REFERENCES projects(name) ON DELETE CASCADE,
        name         TEXT NOT NULL,
        status       TEXT NOT NULL DEFAULT 'draft'
                     CHECK (status IN ('draft', 'in_review', 'approved', 'rejected', 'released')),
        notes        TEXT NOT NULL DEFAULT '',
        repo         TEXT,
        git_ref      TEXT,
        thread_id    TEXT,                      -- the mail thread of the review
        created_by   TEXT NOT NULL,
        created_at   TEXT NOT NULL,
        updated_at   TEXT NOT NULL,
        submitted_at TEXT,
        submitted_by TEXT,
        decided_at   TEXT,
        decided_by   TEXT,
        released_at  TEXT,
        released_by  TEXT,
        UNIQUE (project, name)
    );
    CREATE TABLE version_cards (
        version TEXT NOT NULL REFERENCES versions(id) ON DELETE CASCADE,
        card    TEXT NOT NULL REFERENCES cards(id) ON DELETE CASCADE,
        PRIMARY KEY (version, card)
    );
    CREATE TABLE version_artifacts (
        version    TEXT NOT NULL REFERENCES versions(id) ON DELETE CASCADE,
        artifact   INTEGER NOT NULL REFERENCES artifacts(pk) ON DELETE CASCADE,
        created_by TEXT NOT NULL,
        created_at TEXT NOT NULL,
        PRIMARY KEY (version, artifact)
    );
    CREATE INDEX version_artifacts_artifact ON version_artifacts(artifact);
    CREATE TABLE version_reviews (
        id         TEXT PRIMARY KEY,            -- ULID
        version    TEXT NOT NULL REFERENCES versions(id) ON DELETE CASCADE,
        author     TEXT NOT NULL,
        decision   TEXT NOT NULL CHECK (decision IN ('submitted', 'approved', 'rejected', 'released')),
        comment    TEXT,
        created_at TEXT NOT NULL
    );
    CREATE INDEX version_reviews_version ON version_reviews(version, created_at);
    "#,
    // 4: subscriptions, quota probes, usage accounting, login sessions
    // (PLAN §16).
    r#"
    -- Subscriptions: from `subscriptions/*.toml` (`file`) or the API and the
    -- login wizard (`api`). The probe and login columns are daemon state.
    CREATE TABLE subscriptions (
        name            TEXT PRIMARY KEY,
        provider        TEXT NOT NULL,
        plan            TEXT,
        harnesses       TEXT NOT NULL DEFAULT '[]',   -- JSON array of harness ids
        probe           TEXT,                         -- probe name, NULL: local estimate only
        alert_at        INTEGER NOT NULL DEFAULT 80,
        source          TEXT NOT NULL DEFAULT 'api' CHECK (source IN ('file', 'api')),
        env_var         TEXT,                         -- set when an API key is stored
        login_status    TEXT NOT NULL DEFAULT 'unknown'
                        CHECK (login_status IN ('logged_in', 'expiring', 'expired', 'missing', 'unknown')),
        verified_at     TEXT,
        probe_status    TEXT NOT NULL DEFAULT 'none' CHECK (probe_status IN ('ok', 'failed', 'none')),
        probe_error     TEXT,
        last_probe_at   TEXT,
        last_success_at TEXT,
        created_at      TEXT NOT NULL,
        updated_at      TEXT NOT NULL
    );

    -- One row per window per probe run.
    CREATE TABLE quota_samples (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        subscription TEXT NOT NULL REFERENCES subscriptions(name) ON DELETE CASCADE,
        window       TEXT NOT NULL,                   -- `5h`, `7d`, `monthly`, ...
        used_pct     REAL NOT NULL,
        resets_at    TEXT,
        raw          TEXT NOT NULL DEFAULT '{}',      -- JSON of the provider window
        at           TEXT NOT NULL
    );
    CREATE INDEX quota_samples_sub ON quota_samples(subscription, window, at);

    -- Local accounting: one row per API response. The unique index is the
    -- dedupe rule of docs/compat.md: count each message id once per agent.
    CREATE TABLE usage_samples (
        id            INTEGER PRIMARY KEY AUTOINCREMENT,
        agent         TEXT NOT NULL,                  -- agent ULID; no foreign key, history stays
        project       TEXT NOT NULL,
        subscription  TEXT,
        model         TEXT NOT NULL DEFAULT '',
        input_tokens  INTEGER NOT NULL DEFAULT 0,
        output_tokens INTEGER NOT NULL DEFAULT 0,
        cache_read    INTEGER NOT NULL DEFAULT 0,
        cache_write   INTEGER NOT NULL DEFAULT 0,
        message_id    TEXT NOT NULL,
        turn_seq      INTEGER,                        -- `seq` of the agent's last `turn_start` event
        session       TEXT,                           -- the harness session id
        at            TEXT NOT NULL
    );
    CREATE UNIQUE INDEX usage_samples_dedupe ON usage_samples(agent, message_id);
    CREATE INDEX usage_samples_at ON usage_samples(at);
    CREATE INDEX usage_samples_sub ON usage_samples(subscription, at);
    CREATE INDEX usage_samples_project ON usage_samples(project, at);

    -- Dashboard login flows (PLAN §16.1). The pasted code is never stored.
    CREATE TABLE login_sessions (
        id           TEXT PRIMARY KEY,                -- ULID
        kind         TEXT NOT NULL CHECK (kind IN ('claude', 'codex', 'copilot', 'pi')),
        provider     TEXT,
        state        TEXT NOT NULL
                     CHECK (state IN ('starting', 'waiting', 'verifying', 'done', 'failed', 'cancelled', 'expired')),
        url          TEXT,
        device_code  TEXT,
        subscription TEXT,                            -- the definition made or updated
        started_at   TEXT NOT NULL,
        finished_at  TEXT,
        error        TEXT
    );

    -- Agents whose new turns wait for a quota reset or a daily budget.
    -- `cause` is `quota:<subscription>` or `budget:<project>`.
    CREATE TABLE quota_holds (
        agent      TEXT NOT NULL,
        cause      TEXT NOT NULL,
        until      TEXT,
        reason     TEXT NOT NULL,
        enforced   INTEGER NOT NULL DEFAULT 0,        -- 1: the supervisor holds the turns
        created_at TEXT NOT NULL,
        PRIMARY KEY (agent, cause)
    );

    -- Alerts already sent, so each window period alerts once.
    CREATE TABLE quota_alerts (
        scope  TEXT NOT NULL,                         -- subscription name or `project:<name>`
        window TEXT NOT NULL,
        period TEXT NOT NULL,                         -- the reset time, or the UTC day
        level  TEXT NOT NULL CHECK (level IN ('alert', 'exhausted')),
        at     TEXT NOT NULL,
        PRIMARY KEY (scope, window, period, level)
    );
    "#,
];

/// The shared database handle. One connection behind a mutex; every call
/// runs on the blocking pool, so the async runtime never waits on SQLite.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
    hub: EventHub,
}

impl Db {
    pub fn new(conn: Connection, hub: EventHub) -> Self {
        Self {
            conn: Arc::new(Mutex::new(conn)),
            hub,
        }
    }

    pub fn hub(&self) -> &EventHub {
        &self.hub
    }

    /// Run a read on the blocking pool.
    pub async fn read<T, F>(&self, f: F) -> ApiResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> ApiResult<T> + Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = match conn.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            f(&guard)
        })
        .await
        .map_err(|e| ApiError::internal(format!("database task failed: {e}")))?
    }

    /// Run a write in one transaction. `actor` labels the events and audit
    /// rows that the write makes. The transaction commits when `f` returns
    /// `Ok`; then the new events go to the subscribers.
    pub async fn write<T, F>(&self, actor: impl Into<String>, f: F) -> ApiResult<T>
    where
        T: Send + 'static,
        F: for<'c> FnOnce(&mut Tx<'c>) -> ApiResult<T> + Send + 'static,
    {
        let conn = self.conn.clone();
        let actor = actor.into();
        let (value, events) = tokio::task::spawn_blocking(move || {
            let mut guard = match conn.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            let tx = guard.transaction()?;
            let mut t = Tx {
                tx,
                actor,
                events: Vec::new(),
            };
            let value = f(&mut t)?;
            let Tx { tx, events, .. } = t;
            tx.commit()?;
            Ok::<_, ApiError>((value, events))
        })
        .await
        .map_err(|e| ApiError::internal(format!("database task failed: {e}")))??;
        for e in events {
            self.hub.publish(e);
        }
        Ok(value)
    }
}

/// A write transaction with the actor and the events it made.
pub struct Tx<'c> {
    tx: rusqlite::Transaction<'c>,
    /// `operator`, `agent:<ulid>`, or `system`.
    pub actor: String,
    events: Vec<StoredEvent>,
}

impl<'c> std::ops::Deref for Tx<'c> {
    type Target = rusqlite::Transaction<'c>;
    fn deref(&self) -> &Self::Target {
        &self.tx
    }
}

impl Tx<'_> {
    /// Append an event to the log. Subscribers get it after the commit.
    pub fn emit(&mut self, draft: EventDraft) -> ApiResult<StoredEvent> {
        let event = crate::events::insert(&self.tx, &self.actor, draft)?;
        self.events.push(event.clone());
        Ok(event)
    }

    /// Write an audit row for a privileged action.
    pub fn audit(
        &self,
        action: &str,
        target: &str,
        project: Option<&str>,
        detail: serde_json::Value,
    ) -> ApiResult<()> {
        self.tx.execute(
            "INSERT INTO audit (id, actor, action, target, project, detail, at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                ulid::Ulid::new().to_string(),
                self.actor,
                action,
                target,
                project,
                detail.to_string(),
                now()
            ],
        )?;
        Ok(())
    }
}

/// The current time as stored: RFC 3339, UTC, milliseconds.
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

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
        for table in [
            "projects",
            "agents",
            "sessions",
            "messages",
            "events",
            "repos",
            "roles",
            "audit",
            "boards",
            "board_columns",
            "cards",
            "card_deps",
            "card_comments",
            "card_links",
            "memories",
            "memory_sources",
            "memory_links",
            "memories_fts",
            "wiki_pages",
            "wiki_revisions",
            "wiki_links",
            "wiki_fts",
            "artifacts",
            "artifact_links",
            "versions",
            "version_cards",
            "version_artifacts",
            "version_reviews",
            "subscriptions",
            "quota_samples",
            "usage_samples",
            "login_sessions",
            "quota_holds",
            "quota_alerts",
        ] {
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
    fn migration_2_keeps_old_events() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute_batch(
            "INSERT INTO agents (id, name, project, harness, workspace, created_at, updated_at)
               VALUES ('A1', 'a', 'p', 'claude', '/w', 't', 't');
             INSERT INTO events (id, agent_id, at, type, payload)
               VALUES ('E1', 'A1', 't', 'turn_end', '{}');",
        )
        .unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len() - 1);
        let (seq, kind): (i64, String) = conn
            .query_row("SELECT seq, kind FROM events WHERE id = 'E1'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((seq, kind.as_str()), (1, "turn_end"));
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
