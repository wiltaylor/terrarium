//! SQLite storage and schema migrations.
//!
//! The frozen legacy migrations use `PRAGMA user_version`. New migrations
//! live in `migrations/*.sql` and are tracked by filename, not position.
//! Never edit a migration that has shipped; add a uniquely named file.

use std::borrow::Cow;
use std::cell::{OnceCell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use rusqlite::{Connection, TransactionBehavior};
use terrarium_core::redact::{Redactor, RedactorHandle, View};

use crate::error::{ApiError, ApiResult};
use crate::events::{EventDraft, EventHub, StoredEvent};
use crate::store::audit::AuditEntry;

// `NAMED_MIGRATIONS`: `migrations/*.sql` in filename order, compiled in by
// build.rs (TER-492).
include!(concat!(env!("OUT_DIR"), "/named_migrations.rs"));

#[cfg(test)]
mod release_migrations;

/// Frozen legacy migrations. Index + 1 is the schema version.
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
    // 5: host agents and targets (PLAN §19, docs/host-protocol.md).
    r#"
    -- Host agents. Only a hash of the per-host token is stored.
    CREATE TABLE hosts (
        name         TEXT PRIMARY KEY,
        id           TEXT NOT NULL UNIQUE,            -- ULID, `host_id` in `welcome`
        token_hash   TEXT NOT NULL UNIQUE,            -- SHA-256 hex of the token
        created_by   TEXT NOT NULL,
        capabilities TEXT NOT NULL DEFAULT '{}',      -- JSON HostCapabilities from `hello`
        os           TEXT,
        arch         TEXT,
        version      TEXT,
        status       TEXT NOT NULL DEFAULT 'offline' CHECK (status IN ('online', 'offline')),
        last_seen    TEXT,
        created_at   TEXT NOT NULL
    );

    -- Targets: from `targets/<name>.toml` (source `file`) or the API.
    CREATE TABLE targets (
        name       TEXT PRIMARY KEY,
        project    TEXT,                              -- NULL: a global target
        kind       TEXT NOT NULL CHECK (kind IN ('host', 'ssh', 'docker', 'vmlab')),
        def        TEXT NOT NULL,                     -- JSON TargetDef
        source     TEXT NOT NULL DEFAULT 'api' CHECK (source IN ('file', 'api')),
        state      TEXT NOT NULL DEFAULT 'unknown',
        snapshots  TEXT NOT NULL DEFAULT '[]',        -- JSON array of snapshot names
        state_at   TEXT,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
    CREATE INDEX targets_project ON targets(project);
    "#,
    // 6: mail from `system`, the daemon (quota alerts, board notices). A
    // CHECK cannot change in place, so the messages table is rebuilt. And
    // the agent flag `new_session_on_prompt_change`.
    r#"
    CREATE TABLE messages_v6 (
        id           TEXT PRIMARY KEY,          -- ULID
        thread_id    TEXT NOT NULL,
        from_agent   TEXT NOT NULL,             -- agent id, `operator`, `system`, or `project:<name>`
        to_agent     TEXT NOT NULL,             -- agent id, `operator`, or `project:<name>`
        subject      TEXT,
        body         TEXT NOT NULL,
        state        TEXT NOT NULL DEFAULT 'queued'
                     CHECK (state IN ('queued', 'delivered', 'read', 'replied')),
        created_at   TEXT NOT NULL,
        delivered_at TEXT,
        read_at      TEXT,
        from_kind    TEXT NOT NULL DEFAULT 'agent'
                     CHECK (from_kind IN ('agent', 'project', 'operator', 'system')),
        to_kind      TEXT NOT NULL DEFAULT 'agent'
                     CHECK (to_kind IN ('agent', 'project', 'operator')),
        in_reply_to  TEXT
    );
    INSERT INTO messages_v6 (id, thread_id, from_agent, to_agent, subject, body, state,
                             created_at, delivered_at, read_at, from_kind, to_kind, in_reply_to)
        SELECT id, thread_id, from_agent, to_agent, subject, body, state,
               created_at, delivered_at, read_at, from_kind, to_kind, in_reply_to
        FROM messages;
    DROP TABLE messages;
    ALTER TABLE messages_v6 RENAME TO messages;
    CREATE INDEX messages_to ON messages(to_agent, state);
    CREATE INDEX messages_thread ON messages(thread_id, created_at);
    CREATE INDEX messages_from ON messages(from_agent);

    -- A fresh session, not a resume, when the rendered prompt changed and
    -- the harness keeps the prompt of a session (Codex).
    ALTER TABLE agents ADD COLUMN new_session_on_prompt_change INTEGER NOT NULL DEFAULT 1;
    "#,
    // 7: the seq of the last runner event that the daemon recorded for each
    // agent (`events.jsonl` line number, docs/runner.md "Event seq"). A
    // reconnect to a runner process replays from it, and the event pump
    // drops a record at or below it.
    r#"
    ALTER TABLE agents ADD COLUMN runner_seq INTEGER NOT NULL DEFAULT 0;
    "#,
    // 8: per-card usage attribution (PLAN §16.2). The card that the
    // sample's agent had in `in_progress`, assigned to itself, when the
    // collector stored the sample. No foreign key: history stays.
    r#"
    ALTER TABLE usage_samples ADD COLUMN card_id TEXT;
    CREATE INDEX usage_samples_card ON usage_samples(card_id, at);
    "#,
    // 9: fleet files (docs/fleet.md). The fleet tables come into the
    // schema; before, `fleet.rs` made them with `CREATE TABLE IF NOT
    // EXISTS`, so a database can have them already. And `source` can be
    // `fleet:<name>` on projects, agents, targets, and subscriptions.
    //
    // A CHECK cannot change in place. A table rebuild (as in migration 6)
    // is not safe here: `DROP TABLE` deletes the rows first, and the
    // `ON DELETE CASCADE` of boards, cards, wiki pages, repos, sessions,
    // roles, and quota samples would delete theirs. A migration runs in a
    // transaction, where `PRAGMA foreign_keys` has no effect. So each
    // `source` column is replaced in place: add the new column, copy,
    // drop the old one (its own CHECK goes with it), rename. The column
    // moves to the end of the table; the store reads columns by name.
    r#"
    CREATE TABLE IF NOT EXISTS fleets (
        name        TEXT PRIMARY KEY,
        description TEXT,
        origin      TEXT NOT NULL,          -- `api` or `file:<path in the config dir>`
        content     TEXT NOT NULL,          -- the fleet file as last applied
        applied_at  TEXT NOT NULL,
        applied_by  TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS fleet_objects (
        kind       TEXT NOT NULL,           -- subscription, project, target, agent
        key        TEXT NOT NULL,           -- the name; `project/name` for an agent
        fleet      TEXT NOT NULL,
        project    TEXT,
        applied    TEXT NOT NULL,           -- JSON: the object right after the last apply
        applied_at TEXT NOT NULL,
        PRIMARY KEY (kind, key)
    );
    CREATE INDEX IF NOT EXISTS fleet_objects_fleet ON fleet_objects(fleet);

    ALTER TABLE projects ADD COLUMN source_v9 TEXT NOT NULL DEFAULT 'api'
        CHECK (source_v9 IN ('file', 'api') OR source_v9 GLOB 'fleet:?*');
    UPDATE projects SET source_v9 = source;
    ALTER TABLE projects DROP COLUMN source;
    ALTER TABLE projects RENAME COLUMN source_v9 TO source;

    ALTER TABLE agents ADD COLUMN source_v9 TEXT NOT NULL DEFAULT 'api'
        CHECK (source_v9 IN ('file', 'api') OR source_v9 GLOB 'fleet:?*');
    UPDATE agents SET source_v9 = source;
    ALTER TABLE agents DROP COLUMN source;
    ALTER TABLE agents RENAME COLUMN source_v9 TO source;

    ALTER TABLE targets ADD COLUMN source_v9 TEXT NOT NULL DEFAULT 'api'
        CHECK (source_v9 IN ('file', 'api') OR source_v9 GLOB 'fleet:?*');
    UPDATE targets SET source_v9 = source;
    ALTER TABLE targets DROP COLUMN source;
    ALTER TABLE targets RENAME COLUMN source_v9 TO source;

    ALTER TABLE subscriptions ADD COLUMN source_v9 TEXT NOT NULL DEFAULT 'api'
        CHECK (source_v9 IN ('file', 'api') OR source_v9 GLOB 'fleet:?*');
    UPDATE subscriptions SET source_v9 = source;
    ALTER TABLE subscriptions DROP COLUMN source;
    ALTER TABLE subscriptions RENAME COLUMN source_v9 TO source;

    -- The objects that a fleet owns show the fleet. A `file` row stays
    -- `file`: a definition file took it over, and the file wins.
    UPDATE projects SET source = 'fleet:' || o.fleet
        FROM fleet_objects o
        WHERE o.kind = 'project' AND o.key = projects.name AND projects.source = 'api';
    UPDATE agents SET source = 'fleet:' || o.fleet
        FROM fleet_objects o
        WHERE o.kind = 'agent' AND o.key = agents.project || '/' || agents.name
          AND agents.source = 'api';
    UPDATE targets SET source = 'fleet:' || o.fleet
        FROM fleet_objects o
        WHERE o.kind = 'target' AND o.key = targets.name AND targets.source = 'api';
    UPDATE subscriptions SET source = 'fleet:' || o.fleet
        FROM fleet_objects o
        WHERE o.kind = 'subscription' AND o.key = subscriptions.name
          AND subscriptions.source = 'api';
    "#,
    // 10: host token hygiene (docs/host-protocol.md "Tokens", security
    // review F2). Where and when the token was used, rotation, and the
    // short-lived enrol token that a host agent swaps for its long-lived
    // token on the first connect.
    r#"
    ALTER TABLE hosts ADD COLUMN first_used_at TEXT;
    ALTER TABLE hosts ADD COLUMN last_used_at TEXT;
    ALTER TABLE hosts ADD COLUMN last_used_from TEXT;
    ALTER TABLE hosts ADD COLUMN rotated_at TEXT;
    ALTER TABLE hosts ADD COLUMN enrol_token_hash TEXT;   -- SHA-256 hex; NULL when none is open
    ALTER TABLE hosts ADD COLUMN enrol_expires_at TEXT;
    CREATE UNIQUE INDEX hosts_enrol_token ON hosts(enrol_token_hash);
    "#,
    // 11: dashboard sessions that a logout ended (`api/session.rs`). The
    // cookie stays stateless; a logout writes its nonce here, and the
    // cookie auth refuses it until `exp` (a Unix time). A row past `exp`
    // is pruned: the cookie has ended anyway.
    r#"
    CREATE TABLE revoked_sessions (
        nonce TEXT PRIMARY KEY,
        exp   INTEGER NOT NULL
    );
    CREATE INDEX revoked_sessions_exp ON revoked_sessions(exp);
    "#,
    // 12: the names of deleted agents, and who read a message. Cards,
    // comments, mail, and authors keep an agent ULID; after `agent rm`,
    // the name resolvers find the name here. `read_by` is the reader's
    // key (`operator`, `system`, or an agent ULID) of the first read.
    r#"
    CREATE TABLE agents_removed (
        id         TEXT PRIMARY KEY,
        project    TEXT NOT NULL,
        name       TEXT NOT NULL,
        removed_at TEXT NOT NULL
    );
    ALTER TABLE messages ADD COLUMN read_by TEXT;
    "#,
    // 13: runtime target leases (docs/api.md §17.7). A lead gives a target
    // to an agent for a time, and takes it back. A lease is active while
    // `revoked_at` is NULL and `expires_at` is NULL or later than now.
    // `end_reason` says how it ended: `revoked`, or `expired` (the expiry
    // timer). `project` is the agent's project. No foreign keys: the
    // history stays after an agent or a target is deleted.
    r#"
    CREATE TABLE target_leases (
        id         TEXT PRIMARY KEY,            -- ULID
        target     TEXT NOT NULL,
        agent      TEXT NOT NULL,               -- agent ULID
        project    TEXT NOT NULL,
        granted_by TEXT NOT NULL,               -- `operator` or `agent:<ulid>`
        granted_at TEXT NOT NULL,
        expires_at TEXT,
        exclusive  INTEGER NOT NULL DEFAULT 0,
        revoked_at TEXT,
        revoked_by TEXT,
        end_reason TEXT CHECK (end_reason IN ('revoked', 'expired')),
        reason     TEXT
    );
    CREATE INDEX target_leases_target ON target_leases(target, revoked_at);
    CREATE INDEX target_leases_agent ON target_leases(agent, revoked_at);
    "#,
    // 14: the runtime record of a target (PLAN §11, "mode 1"): the agent
    // workspaces attached to it, the lab instance dir of a `lab_repo`
    // target on its host, and the instances of a `lab_from_workspace`
    // target, one for each holder. JSON; `{}` when nothing runs.
    r#"
    ALTER TABLE targets ADD COLUMN runtime TEXT NOT NULL DEFAULT '{}';
    "#,
    // 15: the push pointers that wait for the next start of an agent that
    // does not run (a card assigned or unblocked, a lease granted or
    // ended), so that they live through a daemon restart. One row for each
    // agent, kind, and key: a newer item for the same key replaces the row
    // and gets a higher `id`. `line` is the text at the park (for a human
    // who reads the table); `data` is the item as JSON. The supervisor
    // deletes a row when it types the pointer, or drops it as no longer
    // current.
    r#"
    CREATE TABLE pending_pointers (
        id         INTEGER PRIMARY KEY,
        agent      TEXT NOT NULL,               -- agent ULID
        kind       TEXT NOT NULL CHECK (kind IN ('card', 'lead', 'lease')),
        key        TEXT NOT NULL,
        line       TEXT NOT NULL,
        data       TEXT NOT NULL,
        created_at TEXT NOT NULL,
        UNIQUE (agent, kind, key)
    );
    "#,
    // 16: the loop guard hold of a message (PLAN §12). `held` is 1 when the
    // guard held the message at insert time (no push pointer), and
    // `held_reason` says why. Messages from before migration 16 have 0.
    r#"
    ALTER TABLE messages ADD COLUMN held INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE messages ADD COLUMN held_reason TEXT;
    "#,
    // 17: schedules (docs/api.md §19). A schedule runs one action on a
    // cron, interval, or one-time spec, as the actor that made it
    // (`run_as`). `scope` is the project, or '' for a global schedule, so
    // that a name is unique within its scope. `anchor_at` is the start of
    // an interval. `skips` counts the runs in a row that the rights check
    // skipped; the third one disables the schedule. Each run is one row of
    // `schedule_runs`; the daily timer keeps the newest 500 of each
    // schedule.
    r#"
    CREATE TABLE schedules (
        id              TEXT PRIMARY KEY,       -- ULID
        name            TEXT NOT NULL,
        project         TEXT REFERENCES projects(name) ON DELETE CASCADE,
        scope           TEXT NOT NULL,          -- project, or '' when global
        enabled         INTEGER NOT NULL DEFAULT 1,
        spec            TEXT NOT NULL,          -- JSON
        action          TEXT NOT NULL,          -- JSON
        catch_up        TEXT NOT NULL DEFAULT 'skip' CHECK (catch_up IN ('skip', 'once')),
        max_concurrency INTEGER NOT NULL DEFAULT 1,
        created_by      TEXT NOT NULL,          -- `operator` or `agent:<ulid>`
        run_as          TEXT NOT NULL,
        anchor_at       TEXT NOT NULL,
        next_run_at     TEXT,
        last_run        TEXT,                   -- JSON
        runs_count      INTEGER NOT NULL DEFAULT 0,
        skips           INTEGER NOT NULL DEFAULT 0,
        disabled_reason TEXT,
        source          TEXT NOT NULL DEFAULT 'api',
        created_at      TEXT NOT NULL,
        updated_at      TEXT NOT NULL,
        UNIQUE (scope, name)
    );
    CREATE INDEX schedules_due ON schedules(enabled, next_run_at);
    CREATE TABLE schedule_runs (
        id          TEXT PRIMARY KEY,           -- ULID
        schedule    TEXT NOT NULL REFERENCES schedules(id) ON DELETE CASCADE,
        trigger     TEXT NOT NULL CHECK (trigger IN ('cron', 'manual', 'catch_up')),
        status      TEXT NOT NULL CHECK (status IN ('running', 'ok', 'failed', 'skipped')),
        started_at  TEXT NOT NULL,
        finished_at TEXT,
        summary     TEXT NOT NULL DEFAULT '',
        output      TEXT NOT NULL DEFAULT '{}', -- JSON
        error       TEXT,
        actor       TEXT NOT NULL               -- who triggered it: run_as, or the caller of run-now
    );
    CREATE INDEX schedule_runs_schedule ON schedule_runs(schedule, id);
    "#,
    // 18: the login state `api_key` (PLAN §16): a stored Z.ai key that no
    // harness status command can check. SQLite cannot change a CHECK, so
    // the column is made again, as migration 9 did for `source`.
    r#"
    ALTER TABLE subscriptions ADD COLUMN login_status_v18 TEXT NOT NULL DEFAULT 'unknown'
        CHECK (login_status_v18 IN ('logged_in', 'expiring', 'expired', 'missing', 'unknown', 'api_key'));
    UPDATE subscriptions SET login_status_v18 = login_status;
    ALTER TABLE subscriptions DROP COLUMN login_status;
    ALTER TABLE subscriptions RENAME COLUMN login_status_v18 TO login_status;
    "#,
    // 19: the delivery log of typed turns (docs/api.md §6.6). The daemon
    // writes one row before it types a turn into a harness: an operator
    // turn (`POST /api/agents/{id}/send`), a push pointer, or a recap. The
    // transcript tags a user text whose hash is here with the row's origin.
    // `hash` is the SHA-256 of the text with its whitespace collapsed, not
    // the text. The store keeps the newest 2000 rows of each agent.
    r#"
    CREATE TABLE deliveries (
        id       INTEGER PRIMARY KEY,
        agent_id TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
        origin   TEXT NOT NULL CHECK (origin IN ('operator', 'agent_mail_pointer', 'system', 'user')),
        hash     TEXT NOT NULL,
        at       TEXT NOT NULL
    );
    CREATE INDEX deliveries_agent ON deliveries(agent_id, hash);
    "#,
    // 20: event triggers (PLAN §12c, docs/api.md §19). A trigger is a
    // schedule whose spec is `on`: an event from the log starts its run.
    // `schedule_runs.trigger` gets 'event', and a run keeps the `seq` of
    // its event. `trigger_state` holds the dedupe and escalation counters
    // of each key. `trigger_cursor` (one row) is the last event `seq` that
    // the trigger engine read, so a restart resumes from the log.
    r#"
    ALTER TABLE schedule_runs RENAME TO schedule_runs_v19;
    CREATE TABLE schedule_runs (
        id          TEXT PRIMARY KEY,           -- ULID
        schedule    TEXT NOT NULL REFERENCES schedules(id) ON DELETE CASCADE,
        trigger     TEXT NOT NULL CHECK (trigger IN ('cron', 'manual', 'catch_up', 'event')),
        status      TEXT NOT NULL CHECK (status IN ('running', 'ok', 'failed', 'skipped')),
        started_at  TEXT NOT NULL,
        finished_at TEXT,
        summary     TEXT NOT NULL DEFAULT '',
        output      TEXT NOT NULL DEFAULT '{}', -- JSON
        error       TEXT,
        actor       TEXT NOT NULL,
        event_seq   INTEGER                     -- the event of a 'event' run
    );
    INSERT INTO schedule_runs (id, schedule, trigger, status, started_at, finished_at, summary, output, error, actor)
        SELECT id, schedule, trigger, status, started_at, finished_at, summary, output, error, actor FROM schedule_runs_v19;
    DROP TABLE schedule_runs_v19;
    CREATE INDEX schedule_runs_schedule ON schedule_runs(schedule, id);
    CREATE TABLE trigger_state (
        schedule    TEXT NOT NULL REFERENCES schedules(id) ON DELETE CASCADE,
        key         TEXT NOT NULL,              -- the rendered dedupe or escalation key
        last_fired  TEXT,                       -- the last run for this key (dedupe window)
        failures    INTEGER NOT NULL DEFAULT 0, -- matches in a row (escalation)
        escalated   INTEGER NOT NULL DEFAULT 0,
        updated_at  TEXT NOT NULL,
        PRIMARY KEY (schedule, key)
    );
    CREATE TABLE trigger_cursor (
        id  INTEGER PRIMARY KEY CHECK (id = 1),
        seq INTEGER NOT NULL
    );
    "#,
    // 21: event sources (PLAN §12c, docs/api.md §20). A source is one
    // webhook sender (GitHub, Forgejo, or a generic script) that posts to
    // the ingest listener. `secret` is the HMAC secret; the API shows it
    // once, at create and at rotate. `scope` is the project, or '' for a
    // global source. `event_deliveries` drops a redelivered webhook; the
    // daily prune keeps 7 days.
    r#"
    CREATE TABLE event_sources (
        id          TEXT PRIMARY KEY,           -- ULID
        name        TEXT NOT NULL,
        project     TEXT REFERENCES projects(name) ON DELETE CASCADE,
        scope       TEXT NOT NULL,              -- project, or '' when global
        kind        TEXT NOT NULL CHECK (kind IN ('github', 'forgejo', 'generic')),
        secret      TEXT NOT NULL,
        enabled     INTEGER NOT NULL DEFAULT 1,
        created_by  TEXT NOT NULL,
        last_seen   TEXT,
        last_error  TEXT,
        received    INTEGER NOT NULL DEFAULT 0,
        created_at  TEXT NOT NULL,
        updated_at  TEXT NOT NULL,
        UNIQUE (scope, name)
    );
    CREATE TABLE event_deliveries (
        source      TEXT NOT NULL REFERENCES event_sources(id) ON DELETE CASCADE,
        delivery    TEXT NOT NULL,
        at          TEXT NOT NULL,
        PRIMARY KEY (source, delivery)
    );
    "#,
    // 22: agent templates (PLAN §5b, docs/api.md §21). A template is the
    // settings of an agent without its name and project; a create copies
    // them. `scope` is the project, or '' for a global template.
    // `revision` goes up at each change. `agents.template` is the
    // template that a create used (JSON `{id, name, project, revision}`),
    // for display only: the agent keeps no live link.
    r#"
    CREATE TABLE agent_templates (
        id          TEXT PRIMARY KEY,           -- ULID
        name        TEXT NOT NULL,
        project     TEXT REFERENCES projects(name) ON DELETE CASCADE,
        scope       TEXT NOT NULL,              -- project, or '' when global
        description TEXT NOT NULL DEFAULT '',
        spec        TEXT NOT NULL,              -- JSON: an agent definition without name and project
        revision    INTEGER NOT NULL DEFAULT 1,
        created_by  TEXT NOT NULL,
        source      TEXT NOT NULL DEFAULT 'api', -- 'api', 'file', or 'fleet:<name>'
        created_at  TEXT NOT NULL,
        updated_at  TEXT NOT NULL,
        UNIQUE (scope, name)
    );
    ALTER TABLE agents ADD COLUMN template TEXT;
    "#,
    // 23: the agent's own base system prompt (PLAN §8). NULL: the harness
    // keeps its default prompt, and the rendered template is appended.
    r#"
    ALTER TABLE agents ADD COLUMN system_prompt TEXT;
    "#,
    // 24: waits and watches (PLAN §12d, docs/api.md §6.7). The status
    // `waiting` is new: the column is made again with the new CHECK (the
    // way of migration 9). A wait is one thing that an agent waits on:
    // another agent, a process (an exec session), a time, or a card.
    // `target_agent` is the agent of an `agent` wait, for the cycle check
    // and the lookup at each status change. `notified_at` is set when the
    // wake mail went out. A watch mails its watcher (an agent ULID or
    // `operator`) when the watched agent goes to one of `statuses`. No
    // foreign keys: the rows of a deleted agent are closed or removed in
    // `store::waits::on_agent_deleted`.
    r#"
    ALTER TABLE agents ADD COLUMN status_v24 TEXT NOT NULL DEFAULT 'stopped'
        CHECK (status_v24 IN ('starting', 'idle', 'waiting', 'working', 'blocked',
                              'error', 'stopped', 'restarting'));
    UPDATE agents SET status_v24 = status;
    ALTER TABLE agents DROP COLUMN status;
    ALTER TABLE agents RENAME COLUMN status_v24 TO status;

    CREATE TABLE waits (
        id           TEXT PRIMARY KEY,          -- ULID
        agent        TEXT NOT NULL,             -- the waiter, an agent ULID
        project      TEXT NOT NULL,             -- the waiter's project
        kind         TEXT NOT NULL CHECK (kind IN ('agent', 'process', 'time', 'card')),
        spec         TEXT NOT NULL,             -- JSON
        target_agent TEXT,                      -- the agent of an `agent` wait
        summary      TEXT NOT NULL,
        state        TEXT NOT NULL DEFAULT 'open'
                     CHECK (state IN ('open', 'resolved', 'timed_out', 'cancelled', 'failed')),
        reason       TEXT,                      -- the caller's note
        created_by   TEXT NOT NULL,
        created_at   TEXT NOT NULL,
        timeout_at   TEXT,
        due_at       TEXT,                      -- a `time` wait
        resolved_at  TEXT,
        result       TEXT,                      -- JSON
        notified_at  TEXT
    );
    CREATE INDEX waits_agent ON waits(agent, state);
    CREATE INDEX waits_target ON waits(target_agent, state);
    CREATE INDEX waits_open ON waits(state, kind);

    CREATE TABLE watches (
        id            TEXT PRIMARY KEY,         -- ULID
        watcher       TEXT NOT NULL,            -- an agent ULID or `operator`
        agent         TEXT NOT NULL,            -- the watched agent ULID
        project       TEXT NOT NULL,            -- the watched agent's project
        statuses      TEXT NOT NULL,            -- JSON array of statuses
        once          INTEGER NOT NULL DEFAULT 0,
        debounce      INTEGER NOT NULL DEFAULT 1,
        created_by    TEXT NOT NULL,
        created_at    TEXT NOT NULL,
        fired         INTEGER NOT NULL DEFAULT 0,
        last_fired_at TEXT
    );
    CREATE INDEX watches_agent ON watches(agent);
    CREATE INDEX watches_watcher ON watches(watcher);
    "#,
    // 25: the target kinds `vnc`, `rdp`, and `desktop` (PLAN §19b). A CHECK
    // cannot change in place, so the column is swapped, as in migration 9.
    r#"
    ALTER TABLE targets ADD COLUMN kind_v25 TEXT NOT NULL DEFAULT 'host'
        CHECK (kind_v25 IN ('host', 'ssh', 'docker', 'vmlab', 'vnc', 'rdp', 'desktop'));
    UPDATE targets SET kind_v25 = kind;
    ALTER TABLE targets DROP COLUMN kind;
    ALTER TABLE targets RENAME COLUMN kind_v25 TO kind;
    "#,
    // 26: the account pool of an agent (PLAN §16.3): a JSON array of
    // subscription names that a swap may pick from.
    r#"
    ALTER TABLE agents ADD COLUMN subscription_pool TEXT NOT NULL DEFAULT '[]';
    "#,
    // 27: the default plan account of each harness (PLAN §16.3): the
    // account of a Claude Code, Codex, or Copilot agent with no
    // subscription. One row per harness, so one default per harness. A
    // NULL `subscription` is a default that was deleted or logged out:
    // the daemon picks no new one. No row: the daemon has not looked yet,
    // and the next start makes the oldest account the default
    // (`store::subscriptions::settle_defaults`).
    r#"
    CREATE TABLE account_defaults (
        harness      TEXT PRIMARY KEY CHECK (harness IN ('claude', 'codex', 'copilot')),
        subscription TEXT REFERENCES subscriptions(name) ON DELETE SET NULL,
        updated_at   TEXT NOT NULL
    );
    "#,
    // 28: the persona of an agent (PLAN §8.1): `leader`, `worker`, `none`,
    // or a prompt file name. NULL: by role.
    r#"
    ALTER TABLE agents ADD COLUMN persona TEXT;
    "#,
    // 29: questions to the operator (PLAN §12e, docs/api.md §6.8). A
    // leader asks; the operator (or a sysadmin agent) answers or
    // retracts. `options` is a JSON array of short choice labels;
    // `answer_option` is the 0-based index of the chosen one. The answer
    // mail goes on `thread_id`. `revision` counts the edits. The wait
    // kind `question` is new: the column is made again with the new
    // CHECK (the way of migration 9); its index goes first.
    r#"
    CREATE TABLE questions (
        id             TEXT PRIMARY KEY,        -- ULID
        agent          TEXT NOT NULL,           -- the asking agent ULID
        project        TEXT NOT NULL,
        text           TEXT NOT NULL,
        options        TEXT NOT NULL DEFAULT '[]',
        context        TEXT,                    -- Markdown
        card           TEXT,
        priority       TEXT NOT NULL DEFAULT 'normal'
                       CHECK (priority IN ('normal', 'urgent')),
        status         TEXT NOT NULL DEFAULT 'open'
                       CHECK (status IN ('open', 'answered', 'retracted')),
        answer         TEXT,
        answer_option  INTEGER,
        answered_by    TEXT,                    -- `operator` or an agent ULID
        answered_at    TEXT,
        retract_reason TEXT,
        retracted_by   TEXT,                    -- the actor label
        thread_id      TEXT NOT NULL,
        created_at     TEXT NOT NULL,
        updated_at     TEXT NOT NULL,
        revision       INTEGER NOT NULL DEFAULT 1
    );
    CREATE INDEX questions_agent ON questions(agent, status);
    CREATE INDEX questions_status ON questions(status, project);

    DROP INDEX waits_open;
    ALTER TABLE waits ADD COLUMN kind_v29 TEXT NOT NULL DEFAULT 'time'
        CHECK (kind_v29 IN ('agent', 'process', 'time', 'card', 'question'));
    UPDATE waits SET kind_v29 = kind;
    ALTER TABLE waits DROP COLUMN kind;
    ALTER TABLE waits RENAME COLUMN kind_v29 TO kind;
    CREATE INDEX waits_open ON waits(state, kind);
    "#,
    // 30: notices and the Inbox (PLAN §12e, docs/api.md §6.8, §7.4). A
    // notice is a row of `questions` with `kind = 'notice'`: no options,
    // and the operator acknowledges it (`acknowledged`). The status
    // column is made again with the new CHECK (the way of migration 29);
    // its indexes go first. `released_at` and `released_by` record the
    // release of a message that the loop guard held. `thread_resumes`
    // records a resume of a paused thread with no mail: the loop guard
    // counts only the messages after `after_rowid`. A held message on a
    // thread that a later operator or project-inbox message reset counts
    // as released, so it does not come back as an Inbox item.
    r#"
    DROP INDEX questions_agent;
    DROP INDEX questions_status;
    ALTER TABLE questions ADD COLUMN kind TEXT NOT NULL DEFAULT 'question'
        CHECK (kind IN ('question', 'notice'));
    ALTER TABLE questions ADD COLUMN status_v30 TEXT NOT NULL DEFAULT 'open'
        CHECK (status_v30 IN ('open', 'answered', 'acknowledged', 'retracted'));
    UPDATE questions SET status_v30 = status;
    ALTER TABLE questions DROP COLUMN status;
    ALTER TABLE questions RENAME COLUMN status_v30 TO status;
    CREATE INDEX questions_agent ON questions(agent, status);
    CREATE INDEX questions_status ON questions(status, project);

    ALTER TABLE messages ADD COLUMN released_at TEXT;
    ALTER TABLE messages ADD COLUMN released_by TEXT;
    UPDATE messages SET released_at = created_at, released_by = 'system'
     WHERE held = 1 AND EXISTS (
        SELECT 1 FROM messages r WHERE r.thread_id = messages.thread_id
           AND r.rowid > messages.rowid
           AND (r.from_kind IN ('operator', 'project') OR r.to_kind = 'project'));

    CREATE TABLE thread_resumes (
        thread_id   TEXT NOT NULL,
        after_rowid INTEGER NOT NULL,           -- the last message rowid at the resume
        by          TEXT NOT NULL,              -- the actor label
        at          TEXT NOT NULL
    );
    CREATE INDEX thread_resumes_thread ON thread_resumes(thread_id);
    "#,
    // 31: the origin `agent` in the delivery log (PLAN §8.5): a turn that
    // another agent typed with `POST /api/agents/{id}/send`, tagged
    // `[Agent:<project>/<name>]`. The origin column is made again with the
    // new CHECK (the way of migration 29). No index has the column.
    r#"
    ALTER TABLE deliveries ADD COLUMN origin_v31 TEXT NOT NULL DEFAULT 'user'
        CHECK (origin_v31 IN ('operator', 'agent', 'agent_mail_pointer', 'system', 'user'));
    UPDATE deliveries SET origin_v31 = origin;
    ALTER TABLE deliveries DROP COLUMN origin;
    ALTER TABLE deliveries RENAME COLUMN origin_v31 TO origin;
    "#,
    // 32: the priority `critical` of questions and notices (PLAN §12e,
    // docs/api.md §6.8): the operator must act now. The priority column is
    // made again with the new CHECK (the way of migration 29). No index has
    // the column.
    r#"
    ALTER TABLE questions ADD COLUMN priority_v32 TEXT NOT NULL DEFAULT 'normal'
        CHECK (priority_v32 IN ('normal', 'urgent', 'critical'));
    UPDATE questions SET priority_v32 = priority;
    ALTER TABLE questions DROP COLUMN priority;
    ALTER TABLE questions RENAME COLUMN priority_v32 TO priority;
    "#,
    // 33: the skills of an agent (PLAN §8.6): a JSON array of skill names
    // from the skill library, or `["*"]` for all of them.
    r#"
    ALTER TABLE agents ADD COLUMN skills TEXT NOT NULL DEFAULT '[]';
    "#,
    // 34: one session row per harness session (PLAN §18). A resume of the
    // same harness session reuses its row (`store::agents::
    // set_harness_session`) and counts in `resumes`; `resumed_at` is the
    // start of the last resume. The rows that earlier resumes made merge
    // into the earliest row of each (agent, harness session): the earliest
    // start, the end of the newest row, its events (so its turns), and
    // `resumes` = the other rows.
    r#"
    ALTER TABLE sessions ADD COLUMN resumes INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE sessions ADD COLUMN resumed_at TEXT;
    CREATE TEMP TABLE session_groups AS
        SELECT agent_id, harness_session_id, min(id) AS keep, max(id) AS last,
               count(*) - 1 AS resumes, min(started_at) AS first_start,
               max(started_at) AS last_start, max(current) AS current
        FROM sessions
        WHERE harness_session_id IS NOT NULL
        GROUP BY agent_id, harness_session_id
        HAVING count(*) > 1;
    CREATE TEMP TABLE session_dups AS
        SELECT d.id AS dup, g.keep AS keep
        FROM sessions d
        JOIN session_groups g
          ON g.agent_id = d.agent_id AND g.harness_session_id = d.harness_session_id
        WHERE d.id != g.keep;
    UPDATE sessions SET
        resumes = (SELECT g.resumes FROM session_groups g WHERE g.keep = sessions.id),
        resumed_at = (SELECT g.last_start FROM session_groups g WHERE g.keep = sessions.id),
        started_at = (SELECT g.first_start FROM session_groups g WHERE g.keep = sessions.id),
        current = (SELECT g.current FROM session_groups g WHERE g.keep = sessions.id),
        ended_at = (SELECT CASE WHEN g.current = 1 THEN NULL ELSE l.ended_at END
                    FROM session_groups g JOIN sessions l ON l.id = g.last
                    WHERE g.keep = sessions.id),
        end_reason = (SELECT CASE WHEN g.current = 1 THEN NULL ELSE l.end_reason END
                      FROM session_groups g JOIN sessions l ON l.id = g.last
                      WHERE g.keep = sessions.id)
    WHERE id IN (SELECT keep FROM session_groups);
    UPDATE events SET session_id = (SELECT x.keep FROM session_dups x WHERE x.dup = events.session_id)
    WHERE session_id IN (SELECT dup FROM session_dups);
    DELETE FROM sessions WHERE id IN (SELECT dup FROM session_dups);
    DROP TABLE session_dups;
    DROP TABLE session_groups;
    "#,
    // 35: the secret manager (PLAN §16b). `scope` is `global` or a project
    // name (no foreign key: `global` is no project; a project delete
    // deletes its secrets, `store::projects::delete`). `plain` is the JSON
    // of the plain fields, `secret_fields` the JSON array of the names of
    // the secret fields, and `sealed` the XChaCha20-Poly1305 ciphertext of
    // the JSON of the secret fields (`crate::secrets::crypto`). A NULL
    // `sealed` is a placeholder with no value (a config import, §16b.5).
    // `secret_grants` gives a secret to an agent or a project (§16b.3);
    // `grantee` is `agent:<ulid>` or `project:<name>`.
    r#"
    CREATE TABLE secrets (
        id            TEXT PRIMARY KEY,           -- ULID
        scope         TEXT NOT NULL,
        name          TEXT NOT NULL,
        kind          TEXT NOT NULL CHECK (kind IN
                          ('api_key', 'token', 'github_token', 'login', 'ssh_key', 'generic')),
        plain         TEXT NOT NULL DEFAULT '{}',
        secret_fields TEXT NOT NULL DEFAULT '[]',
        sealed        BLOB,
        created_at    TEXT NOT NULL,
        updated_at    TEXT NOT NULL,
        created_by    TEXT NOT NULL,
        UNIQUE (scope, name)
    );
    CREATE TABLE secret_grants (
        secret_id  TEXT NOT NULL REFERENCES secrets(id) ON DELETE CASCADE,
        grantee    TEXT NOT NULL,
        level      TEXT NOT NULL CHECK (level IN ('use', 'read')),
        env        TEXT,
        created_by TEXT NOT NULL,
        created_at TEXT NOT NULL,
        PRIMARY KEY (secret_id, grantee)
    );
    CREATE INDEX secret_grants_grantee ON secret_grants(grantee);
    "#,
    // 36: the quota notices that the operator got (PLAN §16.2,
    // `crate::quota_notices`). One row for each subscription window and
    // line (`alert_at` or 90) that has notified in the current period.
    // The row goes when the window drops back under the line or its
    // `resets_at` passes; the next crossing notifies again. Only this
    // comment and the table name hold the number: renumber freely.
    r#"
    CREATE TABLE quota_notices (
        subscription TEXT NOT NULL,
        window       TEXT NOT NULL,
        threshold    INTEGER NOT NULL,           -- percent
        resets_at    TEXT,                       -- the reset of the notified period
        used_pct     REAL NOT NULL,
        message      TEXT,                       -- the mail to the operator
        at           TEXT NOT NULL,
        PRIMARY KEY (subscription, window, threshold)
    );
    "#,
    // 37: sessions outlive their agent (PLAN §13, §18). A card comment
    // links to the session that wrote it, and the link must still work
    // after `agent rm`. So `sessions` loses its `ON DELETE CASCADE`: a
    // rebuild, because SQLite cannot drop a foreign key in place. No
    // table refers to `sessions` (the events table has no foreign keys
    // since migration 2), so the `DROP TABLE` deletes nothing else.
    // `agent_id` is an agent, else a tombstone in `agents_removed`.
    // `harness` is the harness of a deleted agent (NULL: the agent's own).
    // `archive` is the copy of the session file under `<state>/archive/`
    // (`crate::session_archive`), relative to the state dir.
    // `card_comments.session_id` is the session of the agent that wrote
    // the comment (NULL: the operator, or no session).
    //
    // Self-contained: when another branch takes number 37, move this
    // entry to the end of the list; it depends on nothing after 35. The
    // test finds it by the name `sessions_rebuilt`, not by its number.
    r#"
    CREATE TABLE sessions_rebuilt (
        id                 TEXT PRIMARY KEY,    -- ULID
        agent_id           TEXT NOT NULL,       -- agents(id) or agents_removed(id)
        harness_session_id TEXT,
        current            INTEGER NOT NULL DEFAULT 0,
        started_at         TEXT NOT NULL,
        ended_at           TEXT,
        end_reason         TEXT,
        resumes            INTEGER NOT NULL DEFAULT 0,
        resumed_at         TEXT,
        harness            TEXT,
        archive            TEXT
    );
    INSERT INTO sessions_rebuilt (id, agent_id, harness_session_id, current, started_at,
                              ended_at, end_reason, resumes, resumed_at)
        SELECT id, agent_id, harness_session_id, current, started_at,
               ended_at, end_reason, resumes, resumed_at
        FROM sessions;
    DROP TABLE sessions;
    ALTER TABLE sessions_rebuilt RENAME TO sessions;
    CREATE INDEX sessions_agent ON sessions(agent_id, started_at);
    ALTER TABLE card_comments ADD COLUMN session_id TEXT;
    CREATE INDEX card_comments_session ON card_comments(session_id);
    "#,
    // 38: who set the `credential` of a repo or a target (PLAN §16b.6a).
    // 1: the operator, a sysadmin, or a definition file; such an object
    // can name a global secret with no grant. 0: a project_admin; a
    // global secret then needs a grant to the project, at save and at
    // each use. A new row is 0 unless the store sets 1. The rows from
    // before are 1: the daemon cannot know who made them, and an upgrade
    // must not break a working repo or target.
    // Self-contained: when another branch takes number 38, move this
    // entry to the end of the list.
    r#"
    ALTER TABLE repos ADD COLUMN credential_admin INTEGER NOT NULL DEFAULT 0;
    UPDATE repos SET credential_admin = 1;
    ALTER TABLE targets ADD COLUMN credential_admin INTEGER NOT NULL DEFAULT 0;
    UPDATE targets SET credential_admin = 1;
    "#,
    // 39: question bundles (PLAN §12e, docs/api.md §6.8). A bundle is a
    // row of `questions` with `kind = 'bundle'`: `text` is the summary,
    // and `items` is a JSON array of `{text, options, context?, answer?,
    // answer_option?}`, 2 to 20 of them. The operator answers all items
    // in one step. `items` is NULL for a question and a notice. The kind
    // column is made again with the new CHECK (the way of migration 29);
    // its indexes go first.
    // Self-contained: when another branch takes number 39, move this
    // entry to the end of the list. The test finds it by the name
    // `kind_bundle`, not by its number.
    r#"
    DROP INDEX questions_agent;
    DROP INDEX questions_status;
    ALTER TABLE questions ADD COLUMN kind_bundle TEXT NOT NULL DEFAULT 'question'
        CHECK (kind_bundle IN ('question', 'notice', 'bundle'));
    UPDATE questions SET kind_bundle = kind;
    ALTER TABLE questions DROP COLUMN kind;
    ALTER TABLE questions RENAME COLUMN kind_bundle TO kind;
    ALTER TABLE questions ADD COLUMN items TEXT;
    CREATE INDEX questions_agent ON questions(agent, status);
    CREATE INDEX questions_status ON questions(status, project);
    "#,
    // 40: the ETA of a card (PLAN §13, docs/api.md §8). `eta_at` is when
    // the work is expected to be done; `eta_set_at` and `eta_set_by` (an
    // address) say when and by whom. `done_at` is when the card last
    // moved to `done`; it is NULL for a card done before this migration,
    // and a reopen clears it.
    // Self-contained: when another branch takes number 40, move this
    // entry to the end of the list.
    r#"
    ALTER TABLE cards ADD COLUMN eta_at TEXT;
    ALTER TABLE cards ADD COLUMN eta_set_at TEXT;
    ALTER TABLE cards ADD COLUMN eta_set_by TEXT;
    ALTER TABLE cards ADD COLUMN done_at TEXT;
    "#,
    // 41: the agent lifecycle (PLAN §18, "Wake, idle stop, and pause").
    // `idle_stop` is the idle stop of the agent: NULL for the global
    // default, `off`, or a duration such as `30m`. `paused` is 1 while
    // nothing may wake the agent. `status_reason` is the reason of the
    // last status change (`idle` for an idle stop). `queued_turns` keeps
    // the turns (`POST /api/agents/{id}/send`) for an agent that does not
    // run, in order, until its wake types them.
    // Self-contained: when another branch takes number 41, move this
    // entry to the end of the list.
    r#"
    ALTER TABLE agents ADD COLUMN idle_stop TEXT;
    ALTER TABLE agents ADD COLUMN paused INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE agents ADD COLUMN status_reason TEXT;
    CREATE TABLE queued_turns (
        id         INTEGER PRIMARY KEY AUTOINCREMENT,
        agent      TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
        text       TEXT NOT NULL,
        created_at TEXT NOT NULL
    );
    CREATE INDEX queued_turns_agent ON queued_turns(agent, id);
    "#,
    // 42: auto compact (PLAN §18, "Auto compact"). `compact_on_idle` (0 or
    // 1) and `compact_max_age` (`off` or a duration such as `24h`) are the
    // settings of the agent: NULL for the global default. `last_turn_at`
    // is the time of the last turn start of the agent.
    // `last_compact_at` is the end of the last compaction (also a failed
    // one), and `last_compact_ok` its result. A compaction runs only
    // after a turn that is newer than the last compaction and the start
    // of the session.
    // Self-contained: when another branch takes number 42, move this
    // entry to the end of the list.
    r#"
    ALTER TABLE agents ADD COLUMN compact_on_idle INTEGER;
    ALTER TABLE agents ADD COLUMN compact_max_age TEXT;
    ALTER TABLE agents ADD COLUMN last_turn_at TEXT;
    ALTER TABLE agents ADD COLUMN last_compact_at TEXT;
    ALTER TABLE agents ADD COLUMN last_compact_ok INTEGER;
    "#,
    // 43: the harness auto-update (PLAN §6, "Auto-update and pins"). One
    // row for each harness that has a policy or a check. No row: the
    // policy is `auto`. `pinned_version` is set only with `pinned`.
    // `previous` is the active version before the last switch (the
    // target of a rollback). `latest`, `checked_at`, `check_result`, and
    // `check_detail` are from the last check. `failed_version` is the
    // version whose failed install was mailed, so a failure mails once.
    // Self-contained: when another branch takes number 43, move this
    // entry to the end of the list.
    r#"
    CREATE TABLE harness_updates (
        harness        TEXT PRIMARY KEY,
        policy         TEXT NOT NULL DEFAULT 'auto' CHECK (policy IN ('auto', 'pinned')),
        pinned_version TEXT,
        previous       TEXT,
        latest         TEXT,
        checked_at     TEXT,
        check_result   TEXT,
        check_detail   TEXT,
        failed_version TEXT,
        updated_at     TEXT NOT NULL
    );
    "#,
    // 44: agent templates are WCL files in the config repo (PLAN §5b,
    // `crate::template_files`). The table is the index of the files.
    // `file_hash` is the SHA-256 of the file that the row holds. NULL: the
    // row has no file yet (a row from before this version, or a write with
    // no config repo); the next load writes its file. The config export
    // has no templates now, so the export fleet of the global objects
    // gives its templates up: they are config templates.
    // Self-contained: when another branch takes number 44, move this
    // entry to the end of the list.
    r#"
    ALTER TABLE agent_templates ADD COLUMN file_hash TEXT;
    UPDATE agent_templates SET source = 'config' WHERE source = 'fleet:terrarium-global';
    DELETE FROM fleet_objects WHERE kind = 'template' AND fleet = 'terrarium-global';
    "#,
    // 45: the DB of a project (PLAN §15.4, `crate::store::records`).
    // `db_types.schema` is `{"fields": [...]}`; `db_records.data` is a JSON
    // object of the field values. The queries read the values with
    // `json_extract`, inside one type, so the indexes are by type.
    // Self-contained: when another branch takes number 45, move this
    // entry to the end of the list.
    r#"
    CREATE TABLE db_types (
        id          TEXT PRIMARY KEY,           -- ULID
        project     TEXT NOT NULL REFERENCES projects(name) ON DELETE CASCADE,
        name        TEXT NOT NULL,
        description TEXT NOT NULL DEFAULT '',
        schema      TEXT NOT NULL CHECK (json_valid(schema)),
        revision    INTEGER NOT NULL DEFAULT 1,
        created_by  TEXT NOT NULL,
        created_at  TEXT NOT NULL,
        updated_at  TEXT NOT NULL,
        UNIQUE (project, name)
    );
    CREATE TABLE db_records (
        id         TEXT PRIMARY KEY,            -- ULID
        project    TEXT NOT NULL REFERENCES projects(name) ON DELETE CASCADE,
        type       TEXT NOT NULL REFERENCES db_types(id) ON DELETE CASCADE,
        data       TEXT NOT NULL CHECK (json_valid(data)),
        revision   INTEGER NOT NULL DEFAULT 1,
        created_by TEXT NOT NULL,
        updated_by TEXT NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
    CREATE INDEX db_records_type ON db_records(type, id);
    CREATE INDEX db_records_project ON db_records(project, type);
    CREATE INDEX db_records_type_created ON db_records(type, created_at);
    CREATE INDEX db_records_type_updated ON db_records(type, updated_at);
    "#,
    // 46: self-hosted model servers and the enabled switch of a
    // subscription (PLAN §16). `selfhosted` is the JSON of
    // `terrarium_core::selfhosted::SelfHosted`. `openai-compatible` rows
    // become `selfhosted`; the start fills their `selfhosted` from the key
    // file (`store::subscriptions::migrate_openai_compatible`).
    // Self-contained: when another branch takes number 46, move this
    // entry to the end of the list.
    r#"
    ALTER TABLE subscriptions ADD COLUMN selfhosted TEXT CHECK (selfhosted IS NULL OR json_valid(selfhosted));
    ALTER TABLE subscriptions ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1;
    UPDATE subscriptions SET provider = 'selfhosted' WHERE provider = 'openai-compatible';
    "#,
    // 47: the display name that the operator gives a subscription
    // (`PUT /api/subscriptions/{name}/label`). NULL: the name from the
    // vendor table (`terrarium_core::providers::display_name`). Daemon
    // state, not part of the definition. Self-contained: when another
    // branch takes number 47, move this entry to the end of the list.
    r#"
    ALTER TABLE subscriptions ADD COLUMN label TEXT;
    "#,
    // 48: configurable board columns (PLAN §13). `title` is the display
    // label (NULL: the name). `role` is `working` (the column of `card
    // claim`) or `blocked` (a move needs a comment); one column of a board
    // for each role. An existing board keeps its columns: `in_progress`
    // takes `working` and `blocked` takes `blocked`. `backlog` goes first
    // and `done` last, and neither keeps a WIP limit. Self-contained: when
    // another branch takes number 48, move this entry to the end of the
    // list.
    r#"
    ALTER TABLE board_columns ADD COLUMN title TEXT;
    ALTER TABLE board_columns ADD COLUMN role TEXT
        CHECK (role IS NULL OR role IN ('working', 'blocked'));
    UPDATE board_columns SET role = 'working' WHERE name = 'in_progress';
    UPDATE board_columns SET role = 'blocked' WHERE name = 'blocked';
    UPDATE board_columns SET wip_limit = NULL WHERE name IN ('backlog', 'done');
    UPDATE board_columns SET position = -1 WHERE name = 'backlog';
    UPDATE board_columns SET position = (SELECT max(b.position) + 1 FROM board_columns b
                                         WHERE b.project = board_columns.project)
        WHERE name = 'done';
    CREATE UNIQUE INDEX board_columns_role ON board_columns(project, role)
        WHERE role IS NOT NULL;
    "#,
    // 49: the archive flag of a version (PLAN §15.3). Not a status, so the
    // review history stays. NULL: not archived. An archived version has no
    // row on the board and is not in the default list. Self-contained: when
    // another branch takes number 49, move this entry to the end of the
    // list.
    r#"
    ALTER TABLE versions ADD COLUMN archived_at TEXT;
    ALTER TABLE versions ADD COLUMN archived_by TEXT;
    "#,
    // 50: test runs (PLAN §15.5, `crate::store::test_runs`). One row for
    // each uploaded test result file, with the counts of its cases.
    // `raw_gz` is the original file, gzip. `seq` orders the cases of a run
    // in file order, from 0. `pk` orders the runs (newest last); `id` is
    // the ULID of the API. A run links to cards and versions of its
    // project. Self-contained: when another branch takes number 50, move
    // this entry to the end of the list.
    r#"
    CREATE TABLE test_runs (
        pk          INTEGER PRIMARY KEY AUTOINCREMENT,
        id          TEXT NOT NULL UNIQUE,       -- ULID
        project     TEXT NOT NULL REFERENCES projects(name) ON DELETE CASCADE,
        name        TEXT NOT NULL,
        format      TEXT NOT NULL,
        tool        TEXT,
        passed      INTEGER NOT NULL,
        failed      INTEGER NOT NULL,
        errored     INTEGER NOT NULL,
        skipped     INTEGER NOT NULL,
        total       INTEGER NOT NULL,
        duration_ms INTEGER,
        started_at  TEXT,
        commit_sha  TEXT,
        branch      TEXT,
        file_name   TEXT NOT NULL,
        raw_size    INTEGER NOT NULL,
        raw_gz      BLOB NOT NULL,
        notes       TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(notes)),
        created_by  TEXT NOT NULL,
        created_at  TEXT NOT NULL
    );
    CREATE INDEX test_runs_project ON test_runs(project, pk);
    CREATE INDEX test_runs_name ON test_runs(project, name, pk);
    CREATE TABLE test_suites (
        run         INTEGER NOT NULL REFERENCES test_runs(pk) ON DELETE CASCADE,
        idx         INTEGER NOT NULL,
        name        TEXT NOT NULL,
        duration_ms INTEGER,
        passed      INTEGER NOT NULL,
        failed      INTEGER NOT NULL,
        errored     INTEGER NOT NULL,
        skipped     INTEGER NOT NULL,
        total       INTEGER NOT NULL,
        PRIMARY KEY (run, idx)
    );
    CREATE TABLE test_cases (
        run         INTEGER NOT NULL REFERENCES test_runs(pk) ON DELETE CASCADE,
        seq         INTEGER NOT NULL,
        suite       INTEGER NOT NULL,
        name        TEXT NOT NULL,
        classname   TEXT,
        file        TEXT,
        line        INTEGER,
        status      TEXT NOT NULL CHECK (status IN ('passed', 'failed', 'errored', 'skipped')),
        duration_ms INTEGER,
        message     TEXT,
        trace       TEXT,
        stdout      TEXT,
        stderr      TEXT,
        PRIMARY KEY (run, seq)
    );
    CREATE TABLE test_run_cards (
        run        INTEGER NOT NULL REFERENCES test_runs(pk) ON DELETE CASCADE,
        card       TEXT NOT NULL REFERENCES cards(id) ON DELETE CASCADE,
        created_by TEXT NOT NULL,
        created_at TEXT NOT NULL,
        PRIMARY KEY (run, card)
    );
    CREATE INDEX test_run_cards_card ON test_run_cards(card, run);
    CREATE TABLE test_run_versions (
        run        INTEGER NOT NULL REFERENCES test_runs(pk) ON DELETE CASCADE,
        version    TEXT NOT NULL REFERENCES versions(id) ON DELETE CASCADE,
        created_by TEXT NOT NULL,
        created_at TEXT NOT NULL,
        PRIMARY KEY (run, version)
    );
    CREATE INDEX test_run_versions_version ON test_run_versions(version, run);
    "#,
    // 51: ephemeral agents (PLAN §18, "Ephemeral agents",
    // `crate::store::spawns`). `agents.owner` is the owner of an
    // ephemeral agent: the ULID of the agent that spawned it, or
    // `operator`; NULL for every other agent. `spawns` keeps one row for
    // each spawn, also after the finish removes the agent: the task, the
    // deadline, the outcome (`done`, `failed`, `timeout`, `owner_gone`,
    // `cancelled`), the result, and the last session (for the session
    // link). `wait_id` is the wait of the owner on the finish, if any.
    // Self-contained: when another branch takes number 51, move this
    // entry to the end of the list.
    r#"
    ALTER TABLE agents ADD COLUMN owner TEXT;
    CREATE INDEX agents_owner ON agents(owner);
    CREATE TABLE spawns (
        id          TEXT PRIMARY KEY,
        owner       TEXT NOT NULL,
        owner_name  TEXT NOT NULL,
        project     TEXT NOT NULL,
        name        TEXT NOT NULL,
        template    TEXT NOT NULL,
        harness     TEXT NOT NULL,
        task        TEXT NOT NULL,
        spawned_by  TEXT NOT NULL,
        spawned_at  TEXT NOT NULL,
        deadline_at TEXT NOT NULL,
        wait_id     TEXT,
        finished_at TEXT,
        outcome     TEXT,
        result      TEXT,
        session_id  TEXT
    );
    CREATE INDEX spawns_owner ON spawns(owner, id);
    CREATE INDEX spawns_open ON spawns(finished_at);
    "#,
    // 52: the pending reset of an agent (PLAN §18, "Compact and new
    // session on request", `crate::reset`): a compaction or a new session
    // that waits for the end of the current turn, as JSON (`kind`, `then`,
    // `recap`, `by`, `actor`, `deferred`, `requested_at`). NULL: none. One
    // for each agent; a new request replaces it. Self-contained: when
    // another branch takes number 52, move this entry to the end of the
    // list.
    r#"
    ALTER TABLE agents ADD COLUMN pending_reset TEXT;
    "#,
    // 53: the default skills of an agent (PLAN §8.6): `skills_off`, a JSON
    // array of the skill names that the agent does not get, and
    // `default_skills`, 0 to turn off every default-on skill of the
    // library. An agent from before keeps its `skills` and gets the
    // default-on skills. Self-contained: when another branch takes number
    // 53, move this entry to the end of the list.
    r#"
    ALTER TABLE agents ADD COLUMN skills_off TEXT NOT NULL DEFAULT '[]';
    ALTER TABLE agents ADD COLUMN default_skills INTEGER NOT NULL DEFAULT 1;
    "#,
    // 54: monitors and the project dashboard (docs/api.md §30,
    // `crate::store::monitors`). `def` is the JSON of the fields that a
    // client writes (`MonitorDef`); `name`, `kind`, and `enabled` are
    // copies for the unique name, the queries, and the due index.
    // `anchor_at` starts the interval; `next_run_at` is NULL when the
    // monitor is disabled. `run_as` is the actor whose rights each run
    // checks. `monitor_results` keeps one row for each run; `pk` orders
    // them. `dashboards` keeps the layout of a project's dashboard (JSON
    // `{blocks}`). Self-contained: when another branch takes number 54,
    // move this entry to the end of the list.
    r#"
    CREATE TABLE monitors (
        id          TEXT PRIMARY KEY,          -- ULID
        project     TEXT NOT NULL REFERENCES projects(name) ON DELETE CASCADE,
        name        TEXT NOT NULL,
        kind        TEXT NOT NULL CHECK (kind IN ('number', 'status', 'text', 'json')),
        enabled     INTEGER NOT NULL DEFAULT 1,
        def         TEXT NOT NULL CHECK (json_valid(def)),
        created_by  TEXT NOT NULL,
        run_as      TEXT NOT NULL,
        created_at  TEXT NOT NULL,
        updated_at  TEXT NOT NULL,
        anchor_at   TEXT NOT NULL,
        next_run_at TEXT,
        skipped     INTEGER NOT NULL DEFAULT 0,
        UNIQUE (project, name)
    );
    CREATE INDEX monitors_due ON monitors(enabled, next_run_at);
    CREATE TABLE monitor_results (
        pk          INTEGER PRIMARY KEY AUTOINCREMENT,
        monitor     TEXT NOT NULL REFERENCES monitors(id) ON DELETE CASCADE,
        at          TEXT NOT NULL,
        status      TEXT NOT NULL CHECK (status IN ('ok', 'warn', 'crit', 'unknown')),
        value       REAL,
        text        TEXT,
        json        TEXT CHECK (json IS NULL OR json_valid(json)),
        duration_ms INTEGER NOT NULL,
        error       TEXT
    );
    CREATE INDEX monitor_results_at ON monitor_results(monitor, at);
    CREATE INDEX monitor_results_pk ON monitor_results(monitor, pk);
    CREATE TABLE dashboards (
        project    TEXT PRIMARY KEY REFERENCES projects(name) ON DELETE CASCADE,
        layout     TEXT NOT NULL CHECK (json_valid(layout)),
        updated_at TEXT NOT NULL,
        updated_by TEXT NOT NULL
    );
    "#,
    // 55: webhook deliveries (docs/api.md §19.5, `crate::store::webhooks`).
    // One row for each delivery of a `webhook` action: the outbox that the
    // delivery worker (`crate::webhooks`) sends from, and the log that
    // the API shows. `body` is the rendered body; the headers, the URL,
    // and the signature come from the trigger's action at each attempt,
    // so no secret value is stored. `status` is `pending` (queued or
    // waiting for a retry at `next_attempt_at`), `ok`, `failed` (no retry:
    // a 4xx, a refused URL), `giving_up` (the retries are used up), or
    // `dropped` (the rate limit: `dropped` counts the events of one
    // minute). `cause` is the `data.cause` of the delivery events.
    // `schedules.webhook_failures` counts the failed attempts in a row;
    // the 20th disables the trigger. The daily prune keeps 7 days.
    // Self-contained: when another branch takes number 55, move this
    // entry to the end of the list.
    r#"
    CREATE TABLE webhook_deliveries (
        id              TEXT PRIMARY KEY,          -- ULID
        schedule        TEXT NOT NULL REFERENCES schedules(id) ON DELETE CASCADE,
        event_seq       INTEGER,
        event_kind      TEXT NOT NULL DEFAULT '',
        test            INTEGER NOT NULL DEFAULT 0,
        redelivery_of   TEXT,
        url             TEXT NOT NULL,             -- for the log: no credentials, no query
        method          TEXT NOT NULL,
        content_type    TEXT NOT NULL,
        body            TEXT NOT NULL,
        format_headers  TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(format_headers)),
        attempt         INTEGER NOT NULL DEFAULT 0,
        status          TEXT NOT NULL CHECK (status IN ('pending', 'ok', 'failed', 'giving_up', 'dropped')),
        http_status     INTEGER,
        duration_ms     INTEGER,
        response        TEXT,
        error           TEXT,
        dropped         INTEGER NOT NULL DEFAULT 0,
        cause           TEXT CHECK (cause IS NULL OR json_valid(cause)),
        next_attempt_at TEXT,
        created_at      TEXT NOT NULL,
        updated_at      TEXT NOT NULL
    );
    CREATE INDEX webhook_deliveries_schedule ON webhook_deliveries(schedule, id);
    CREATE INDEX webhook_deliveries_due ON webhook_deliveries(status, next_attempt_at);
    ALTER TABLE schedules ADD COLUMN webhook_failures INTEGER NOT NULL DEFAULT 0;
    "#,
    // 56: pipelines (docs/api.md §31, `crate::store::pipelines`). `def` is
    // the JSON of the fields that a client writes (`PipelineDef`); `name`,
    // `adapter`, and `enabled` are copies for the unique name, the
    // queries, and the due index. `anchor_at` starts the interval;
    // `next_run_at` is NULL when the pipeline is disabled. `run_as` is the
    // actor whose rights each poll checks. `status` is the status of the
    // newest run; `polled_at` the last poll, `ok_at` the last good one,
    // `error` the error of the last poll. `pipeline_runs` keeps the recent
    // runs of each pipeline, one row for each run ID: `sort_at` (the start,
    // else the first sight) and `ord` (the place in the last poll that had
    // it) order them; `steps` is NULL until the steps are read.
    // Self-contained: when another branch takes number 56, move this
    // entry to the end of the list.
    r#"
    CREATE TABLE pipelines (
        id          TEXT PRIMARY KEY,          -- ULID
        project     TEXT NOT NULL REFERENCES projects(name) ON DELETE CASCADE,
        name        TEXT NOT NULL,
        adapter     TEXT NOT NULL CHECK (adapter IN ('github', 'script')),
        enabled     INTEGER NOT NULL DEFAULT 1,
        def         TEXT NOT NULL CHECK (json_valid(def)),
        created_by  TEXT NOT NULL,
        run_as      TEXT NOT NULL,
        created_at  TEXT NOT NULL,
        updated_at  TEXT NOT NULL,
        anchor_at   TEXT NOT NULL,
        next_run_at TEXT,
        skipped     INTEGER NOT NULL DEFAULT 0,
        status      TEXT,
        polled_at   TEXT,
        ok_at       TEXT,
        error       TEXT,
        UNIQUE (project, name)
    );
    CREATE INDEX pipelines_due ON pipelines(enabled, next_run_at);
    CREATE TABLE pipeline_runs (
        pk          INTEGER PRIMARY KEY AUTOINCREMENT,
        pipeline    TEXT NOT NULL REFERENCES pipelines(id) ON DELETE CASCADE,
        run_id      TEXT NOT NULL,
        name        TEXT,
        status      TEXT NOT NULL CHECK (status IN ('queued', 'running', 'waiting', 'success',
                                                    'failed', 'cancelled', 'skipped', 'unknown')),
        url         TEXT,
        branch      TEXT,
        commit_sha  TEXT,
        actor       TEXT,
        event       TEXT,
        environment TEXT,
        started_at  TEXT,
        finished_at TEXT,
        summary     TEXT,
        steps       TEXT CHECK (steps IS NULL OR json_valid(steps)),
        sort_at     TEXT NOT NULL,
        ord         INTEGER NOT NULL DEFAULT 0,
        first_seen  TEXT NOT NULL,
        changed_at  TEXT NOT NULL,
        seen_at     TEXT NOT NULL,
        UNIQUE (pipeline, run_id)
    );
    CREATE INDEX pipeline_runs_sort ON pipeline_runs(pipeline, sort_at);
    "#,
    // 57: pinned conversations (docs/api.md §6.11, `crate::store::pins`).
    // The agents whose conversation the dashboard keeps in its right-hand
    // side bar, for each owner (`operator`), in `position` order. The
    // foreign key removes the pin of a deleted agent. Self-contained: when
    // another branch takes number 57, move this entry to the end of the
    // list.
    r#"
    CREATE TABLE pinned_agents (
        owner     TEXT NOT NULL,
        agent     TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
        position  INTEGER NOT NULL,
        pinned_at TEXT NOT NULL,
        PRIMARY KEY (owner, agent)
    );
    CREATE INDEX pinned_agents_agent ON pinned_agents(agent);
    "#,
    // 58: git hooks of a repo (PLAN §9, "Git hooks in agent checkouts").
    // `hooks_dir`: a tracked dir of the repo whose files each clone and
    // update copies into `.git/hooks`. `push_deny`: a JSON list of branch
    // globs that the daemon's `pre-push` hook refuses. Self-contained:
    // when another branch takes number 58, move this entry to the end.
    r#"
    ALTER TABLE repos ADD COLUMN hooks_dir TEXT;
    ALTER TABLE repos ADD COLUMN push_deny TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(push_deny));
    "#,
    // 59: the calendar (docs/api.md §33, `crate::store::calendar`).
    // `calendar_events` keeps one row for each event: `project` is NULL
    // for a global event; `start_at` and `end_at` are the first
    // occurrence (UTC); `recurrence` is the JSON rule or NULL; `links` a
    // JSON object; `created_by` and `updated_by` are actor labels.
    // `calendar_assignees` are the agents that get the start and end
    // notices. `calendar_exceptions` names each skipped occurrence by its
    // start. `calendar_watches` are the watchers of a calendar (`agent`
    // NULL: the operator Inbox; `project` NULL: the global calendar).
    // `calendar_notices` keeps one row for each occurrence and edge that
    // the tick handled (`sent`, `late`, `skipped`, `closed`), so a restart
    // sends nothing twice. `calendar_state` keeps the time of the last
    // tick (`tick_at`). Self-contained: when another branch takes number
    // 59, move this entry to the end of the list.
    r#"
    CREATE TABLE calendar_events (
        id          TEXT PRIMARY KEY,          -- ULID
        project     TEXT REFERENCES projects(name) ON DELETE CASCADE,
        title       TEXT NOT NULL,
        description TEXT NOT NULL DEFAULT '',
        kind        TEXT NOT NULL CHECK (kind IN ('window', 'maintenance', 'freeze', 'deploy',
                                                  'task', 'other')),
        start_at    TEXT NOT NULL,
        end_at      TEXT NOT NULL,
        all_day     INTEGER NOT NULL DEFAULT 0,
        tz          TEXT NOT NULL DEFAULT 'UTC',
        recurrence  TEXT CHECK (recurrence IS NULL OR json_valid(recurrence)),
        links       TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(links)),
        created_by  TEXT NOT NULL,
        updated_by  TEXT NOT NULL,
        created_at  TEXT NOT NULL,
        updated_at  TEXT NOT NULL
    );
    CREATE INDEX calendar_events_project ON calendar_events(project, start_at);
    CREATE TABLE calendar_assignees (
        event    TEXT NOT NULL REFERENCES calendar_events(id) ON DELETE CASCADE,
        agent    TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
        added_by TEXT NOT NULL,
        added_at TEXT NOT NULL,
        PRIMARY KEY (event, agent)
    );
    CREATE INDEX calendar_assignees_agent ON calendar_assignees(agent);
    CREATE TABLE calendar_exceptions (
        event      TEXT NOT NULL REFERENCES calendar_events(id) ON DELETE CASCADE,
        occurrence TEXT NOT NULL,
        created_by TEXT NOT NULL,
        created_at TEXT NOT NULL,
        PRIMARY KEY (event, occurrence)
    );
    CREATE TABLE calendar_watches (
        agent      TEXT REFERENCES agents(id) ON DELETE CASCADE,
        project    TEXT REFERENCES projects(name) ON DELETE CASCADE,
        created_by TEXT NOT NULL,
        created_at TEXT NOT NULL
    );
    CREATE UNIQUE INDEX calendar_watches_key
        ON calendar_watches(IFNULL(agent, 'operator'), IFNULL(project, ''));
    CREATE TABLE calendar_notices (
        event      TEXT NOT NULL REFERENCES calendar_events(id) ON DELETE CASCADE,
        occurrence TEXT NOT NULL,
        edge       TEXT NOT NULL CHECK (edge IN ('start', 'end')),
        state      TEXT NOT NULL CHECK (state IN ('sent', 'late', 'skipped', 'closed')),
        at         TEXT NOT NULL,
        PRIMARY KEY (event, occurrence, edge)
    );
    CREATE TABLE calendar_state (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    "#,
    // 60: the git proxy of a repo (issue #11, PLAN §9, `crate::git_proxy`).
    // `push_via`: `proxy` (agent checkouts reach the forge through the
    // daemon's git proxy), `direct`, or NULL (the proxy for an https repo
    // with a token and a `push_deny` list). Self-contained: when another
    // branch takes this number, move this entry to the end of the list.
    r#"
    ALTER TABLE repos ADD COLUMN push_via TEXT CHECK (push_via IN ('proxy', 'direct'));
    "#,
    // 61: the files of a card (docs/api.md §11.6, `crate::store::card_files`).
    // One row for each version of a file. `name_key` is the name in lower
    // case: a name is unique per card with no regard to case. The bytes are
    // in the blob store `<state>/card-files/<sha256>`. `deleted_at` marks a
    // removed file (all its versions); the prune timer deletes such rows
    // after `TERRARIUM_TRASH_DAYS`, then the blobs that no row uses.
    // `created_by` and `deleted_by` are addresses (`operator`, an agent
    // ULID). Self-contained: when another branch takes this number, move
    // this entry to the end of the list.
    r#"
    CREATE TABLE card_files (
        id         TEXT PRIMARY KEY,            -- ULID
        card       TEXT NOT NULL REFERENCES cards(id) ON DELETE CASCADE,
        name       TEXT NOT NULL,
        name_key   TEXT NOT NULL,
        version    INTEGER NOT NULL CHECK (version >= 1),
        size       INTEGER NOT NULL CHECK (size >= 0),
        sha256     TEXT NOT NULL,
        media_type TEXT NOT NULL,
        created_by TEXT NOT NULL,
        created_at TEXT NOT NULL,
        deleted_at TEXT,
        deleted_by TEXT,
        UNIQUE (card, name_key, version)
    );
    CREATE INDEX card_files_sha ON card_files(sha256);
    "#,
    // 62: the owner of a monitor and of a schedule (issue #18, docs/api.md
    // §30.7): `actor` (the runs act as `run_as`), `project` (the rights
    // come from the project), or `system`. `disabled_reason` on a monitor:
    // why the daemon disabled it (its `run_as` agent was deleted).
    // `monitor_results.actor`: the actor of each run, `monitor:<p>/<name>`
    // (the runs write no audit row; the results have a retention). And the
    // grants of a global target to a project for its project-owned
    // monitors or schedules (`crate::store::target_grants`); a project
    // target needs none. Self-contained: when another branch takes this
    // number, move this entry to the end of the list.
    r#"
    ALTER TABLE monitors ADD COLUMN owner TEXT NOT NULL DEFAULT 'actor'
        CHECK (owner IN ('actor', 'project', 'system'));
    ALTER TABLE monitors ADD COLUMN disabled_reason TEXT;
    ALTER TABLE monitor_results ADD COLUMN actor TEXT;
    ALTER TABLE schedules ADD COLUMN owner TEXT NOT NULL DEFAULT 'actor'
        CHECK (owner IN ('actor', 'project', 'system'));
    CREATE TABLE target_grants (
        target     TEXT NOT NULL REFERENCES targets(name) ON DELETE CASCADE,
        project    TEXT NOT NULL REFERENCES projects(name) ON DELETE CASCADE,
        purpose    TEXT NOT NULL CHECK (purpose IN ('monitors', 'schedules')),
        granted_by TEXT NOT NULL,
        granted_at TEXT NOT NULL,
        PRIMARY KEY (target, project, purpose)
    );
    CREATE INDEX target_grants_project ON target_grants(project);
    "#,
    // 63: the self-update (docs/api.md §34, `crate::self_update`). One
    // row for each key: `notified:<version>` (the new-release mail went
    // out), `run:<id>` (the result mail of an updater run went out),
    // `results_baseline` (the first read of the updater history: older
    // runs get no mail), and `auto_window` (the maintenance occurrence of
    // the last auto-update). Self-contained: when another branch takes this
    // number, move this entry to the end of the list.
    r#"
    CREATE TABLE self_update_state (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    "#,
    // 64: the card reminder of an agent (PLAN §18, "Card reminder"):
    // `card_reminder` 0 or 1, NULL for the global default
    // (`TERRARIUM_CARD_REMINDER`, on). Self-contained: when another branch
    // takes this number, move this entry to the end of the list.
    r#"
    ALTER TABLE agents ADD COLUMN card_reminder INTEGER;
    "#,
    // 65: the watched threads of the Inbox (GitHub #24, docs/api.md §6.9,
    // `crate::store::inbox`): a question that the operator asked an agent,
    // or a question of the Inbox that the operator forwarded to one. The
    // watch waits while the newest message of its thread is the
    // operator's. Self-contained: when another branch takes this number,
    // move this entry to the end of the list.
    r#"
    CREATE TABLE inbox_watches (
        thread_id    TEXT PRIMARY KEY,
        agent        TEXT NOT NULL,
        question_id  TEXT,
        message_id   TEXT NOT NULL,
        created_at   TEXT NOT NULL,
        cancelled_at TEXT
    );
    "#,
    // 66: tool proxies (GitHub #21, the wiki page design/tool-proxies,
    // `crate::store::tools`). `tools` keeps each definition as JSON
    // (`ToolDef`) from a `tools/<name>.toml` file (source `file`) or the
    // API (source `api`). `tool_grants` gives a tool to an agent
    // (`agent:<ulid>`) or a project (`project:<name>`), as
    // `secret_grants` does for a secret. Removing a tool removes its
    // grants. Self-contained: when another branch takes this number, move
    // this entry to the end of the list.
    r#"
    CREATE TABLE tools (
        name       TEXT PRIMARY KEY,
        project    TEXT,                              -- NULL: a global tool
        def        TEXT NOT NULL,                     -- JSON ToolDef
        source     TEXT NOT NULL DEFAULT 'api' CHECK (source IN ('file', 'api')),
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
    CREATE INDEX tools_project ON tools(project);
    CREATE TABLE tool_grants (
        tool       TEXT NOT NULL REFERENCES tools(name) ON DELETE CASCADE ON UPDATE CASCADE,
        grantee    TEXT NOT NULL,
        created_by TEXT NOT NULL,
        created_at TEXT NOT NULL,
        PRIMARY KEY (tool, grantee)
    );
    CREATE INDEX tool_grants_grantee ON tool_grants(grantee);
    "#,
    // 67: the approval of a tool run (GitHub #21, design D5, TER-31). A
    // question of the kind `tool_run` asks the operator to run one tool
    // with one argv for one agent. `tool_run` holds `{tool, target, argv,
    // expires_at}` (JSON); `used_at` is set when the approved run starts,
    // so an approval runs once. The kind column is made again with the new
    // CHECK, as migration 39 did; its indexes go first. Self-contained:
    // when another branch takes this number, move this entry to the end of
    // the list. The test finds it by the name `kind_tool_run`.
    r#"
    DROP INDEX questions_agent;
    DROP INDEX questions_status;
    ALTER TABLE questions ADD COLUMN kind_tool_run TEXT NOT NULL DEFAULT 'question'
        CHECK (kind_tool_run IN ('question', 'notice', 'bundle', 'tool_run'));
    UPDATE questions SET kind_tool_run = kind;
    ALTER TABLE questions DROP COLUMN kind;
    ALTER TABLE questions RENAME COLUMN kind_tool_run TO kind;
    ALTER TABLE questions ADD COLUMN tool_run TEXT;
    ALTER TABLE questions ADD COLUMN used_at TEXT;
    CREATE INDEX questions_agent ON questions(agent, status);
    CREATE INDEX questions_status ON questions(status, project);
    "#,
    // 68: project folders of the dashboard sidebar (TER-48, docs/api.md
    // §6.12, `crate::store::project_folders`). Folders nest by
    // `parent_id` (NULL is the root). A project's place is a row in
    // `project_placements`; a project with no row sits at the root. The
    // projects table is not touched: its rows come from definition files.
    // The cascade removes the place of a deleted project. A folder delete
    // moves its content up in the store before the row goes, so the
    // `parent_id` foreign keys have no action. Self-contained: when
    // another branch takes this number, move this entry to the end of the
    // list.
    r#"
    CREATE TABLE project_folders (
        id         TEXT PRIMARY KEY,
        name       TEXT NOT NULL,
        parent_id  TEXT REFERENCES project_folders(id),
        position   INTEGER NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
    CREATE INDEX project_folders_parent ON project_folders(parent_id, position);
    CREATE TABLE project_placements (
        project   TEXT PRIMARY KEY REFERENCES projects(name) ON DELETE CASCADE,
        folder_id TEXT REFERENCES project_folders(id),
        position  INTEGER NOT NULL
    );
    CREATE INDEX project_placements_folder ON project_placements(folder_id, position);
    "#,
    // 69: the role `architect` (TER-47) in the CHECK of `roles.role`. It
    // comes after 68 of TER-48 (project folders): when both branches are
    // merged, this entry is the last. The column is part of the primary
    // key, so the table is made again. No table refers to `roles` (it
    // refers to `agents`), so the drop deletes no other row.
    // Self-contained: when another branch takes this number, move this
    // entry to the end of the list. The test finds it by the name
    // `roles_architect`.
    r#"
    CREATE TABLE roles_architect (
        agent_id   TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
        role       TEXT NOT NULL CHECK (role IN ('project_admin', 'sysadmin', 'architect')),
        project    TEXT,
        granted_by TEXT NOT NULL,
        granted_at TEXT NOT NULL,
        PRIMARY KEY (agent_id, role)
    );
    INSERT INTO roles_architect (agent_id, role, project, granted_by, granted_at)
        SELECT agent_id, role, project, granted_by, granted_at FROM roles;
    DROP TABLE roles;
    ALTER TABLE roles_architect RENAME TO roles;
    "#,
    // 70: the idle alert (TER-67, `crate::idle_alert`). `projects.idle_alert`
    // is the project's threshold (`null`: the default, `off`, or a
    // duration). `cards.idle_since` marks a card whose assignee rests with
    // no wait past that threshold: the start of the rest. Self-contained:
    // when another branch takes this number, move this entry to the end.
    r#"
    ALTER TABLE projects ADD COLUMN idle_alert TEXT;
    ALTER TABLE cards ADD COLUMN idle_since TEXT;
    "#,
    // 71: the job queue of a project (TER-71, `crate::jobs`). A job
    // validates a card commit, or merges a card branch onto `next` and
    // validates the result. Jobs start first in, first out, at most the
    // project's `slots` at a time. `attempt` is 1, or 2 for the
    // one rerun of a failed job. `outcome` says why a job failed (`red`,
    // `conflict`, `error`). `project_delivery` holds the queue config of a
    // project (JSON `DeliveryDef`). Self-contained: when another branch
    // takes this number, move this entry to the end of the list.
    r#"
    CREATE TABLE jobs (
        id          TEXT PRIMARY KEY,
        project     TEXT NOT NULL,
        card        TEXT NOT NULL,
        kind        TEXT NOT NULL CHECK (kind IN ('validate', 'merge')),
        repo        TEXT NOT NULL,
        branch      TEXT NOT NULL,
        commit_sha  TEXT,
        state       TEXT NOT NULL DEFAULT 'queued'
            CHECK (state IN ('queued', 'running', 'passed', 'failed', 'cancelled')),
        attempt     INTEGER NOT NULL DEFAULT 1,
        outcome     TEXT,
        exit_code   INTEGER,
        tail        TEXT,
        head        TEXT,
        by_agent    TEXT,
        created_by  TEXT NOT NULL,
        created_at  TEXT NOT NULL,
        started_at  TEXT,
        ended_at    TEXT
    );
    CREATE INDEX jobs_project_state ON jobs(project, state, created_at);
    CREATE INDEX jobs_card ON jobs(card);
    CREATE TABLE project_delivery (
        project    TEXT PRIMARY KEY REFERENCES projects(name) ON DELETE CASCADE ON UPDATE CASCADE,
        def        TEXT NOT NULL,                     -- JSON DeliveryDef
        updated_by TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
    "#,
    // 72: a host-wide cap on the running jobs of the job queue (TER-97,
    // `crate::jobs`). `jobs.host` is the host that a running job took a
    // place on (set when it starts). `job_hosts` holds the cap of a host:
    // the most jobs of all projects that run on it at once. A host
    // without a row has no cap. Self-contained: when another branch takes
    // this number, move this entry to the end of the list.
    r#"
    ALTER TABLE jobs ADD COLUMN host TEXT;
    CREATE INDEX jobs_host_state ON jobs(host, state);
    CREATE TABLE job_hosts (
        host       TEXT PRIMARY KEY,
        heavy_jobs INTEGER NOT NULL CHECK (heavy_jobs >= 1),
        updated_by TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
    "#,
    // 73: bound ephemeral agents (TER-74, `crate::bound`). `bound_kind`
    // and `bound_id` name the object whose life the agent shares (for
    // example `card`, `TER-74`); NULL: a plain spawn. At most one open
    // spawn per object. `parked_at` and `park_reason`: the agent is parked
    // (stopped, no wake) while its object waits; NULL: not parked.
    // `unpark_at`: an unpark waits for a place of the owner (first in,
    // first out); NULL: none.
    // Self-contained: when another branch takes this number, move this
    // entry to the end of the list.
    r#"
    ALTER TABLE spawns ADD COLUMN bound_kind TEXT;
    ALTER TABLE spawns ADD COLUMN bound_id TEXT;
    ALTER TABLE spawns ADD COLUMN parked_at TEXT;
    ALTER TABLE spawns ADD COLUMN park_reason TEXT;
    ALTER TABLE spawns ADD COLUMN unpark_at TEXT;
    CREATE UNIQUE INDEX spawns_bound_open ON spawns(bound_kind, bound_id)
        WHERE finished_at IS NULL AND bound_kind IS NOT NULL;
    "#,
    // 74: Inbox threads (TER-91, `crate::store::threads`): a conversation
    // of the operator with one agent on one topic. `report_to` is the lead
    // of the scope (D34), set at the open. `status` is `waiting_operator`,
    // `waiting_agent`, or `closed`. `response` is the newest overall
    // response of the operator (JSON `{text, by, at}`); `handoff` is set
    // at the close (JSON). `thread_messages` holds the conversation; the
    // questions of a thread have `questions.thread`. Self-contained: when
    // another branch takes this number, move this entry to the end.
    r#"
    CREATE TABLE threads (
        id         TEXT PRIMARY KEY,
        topic      TEXT NOT NULL,
        scope      TEXT NOT NULL,
        template   TEXT,
        opened_by  TEXT NOT NULL,
        agent      TEXT,
        report_to  TEXT,
        status     TEXT NOT NULL
            CHECK (status IN ('waiting_operator', 'waiting_agent', 'closed')),
        response   TEXT,
        handoff    TEXT,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        closed_at  TEXT,
        closed_by  TEXT
    );
    CREATE INDEX threads_status ON threads(status, updated_at);
    CREATE INDEX threads_agent ON threads(agent);
    CREATE INDEX threads_report_to ON threads(report_to);
    CREATE TABLE thread_messages (
        id      TEXT PRIMARY KEY,
        thread  TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
        author  TEXT NOT NULL,
        kind    TEXT NOT NULL CHECK (kind IN ('message', 'response')),
        text    TEXT NOT NULL,
        at      TEXT NOT NULL
    );
    CREATE INDEX thread_messages_thread ON thread_messages(thread, at);
    ALTER TABLE questions ADD COLUMN thread TEXT;
    CREATE INDEX questions_thread ON questions(thread);
    "#,
    // 75: batch merges (TER-112, `crate::jobs`). `jobs.batch` is the JSON
    // list of the cards of a merge job that merges several cards in order
    // ([{card, branch, by_agent, outcome?, note?}]); `null` for a job of
    // one card. `jobs.parent` is the job that a split of a red batch came
    // from. `jobs.split_path` orders the halves of the splits of one root
    // batch (`0` left, `1` right, per level; empty for no split).
    // Self-contained: when another branch takes this number, move this
    // entry to the end of the list.
    r#"
    ALTER TABLE jobs ADD COLUMN batch TEXT;
    ALTER TABLE jobs ADD COLUMN parent TEXT;
    ALTER TABLE jobs ADD COLUMN split_path TEXT NOT NULL DEFAULT '';
    "#,
    // 76: pin job-host DNS identities across queue ticks and restarts.
    // Follows 74 (TER-91) and 75 (TER-112). Keep this entry self-contained
    // and renumber at merge if the integration order changes.
    r#"
    CREATE TABLE job_host_keys (
        hostname TEXT PRIMARY KEY,
        host_key TEXT NOT NULL
    );
    "#,
    // 77: the addresses of a host agent (TER-105, `crate::store::jobs`).
    // An SSH target whose address is an alias of a host agent counts on
    // that host agent's key for the host cap. `alias` is the address key
    // (no user, no port, lowercase). One host agent per alias. Self-
    // contained: when another branch takes this number, move this entry
    // to the end of the list.
    r#"
    CREATE TABLE job_host_aliases (
        alias      TEXT PRIMARY KEY,
        host_id    TEXT NOT NULL,
        updated_by TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
    CREATE INDEX job_host_aliases_host ON job_host_aliases(host_id);
    "#,
    // 78: the close of an Inbox thread (TER-95, `crate::thread_close`).
    // `close_proposal` is the agent's proposed handoff (JSON `{summary,
    // links, by, at}`) that the operator confirms; NULL: none.
    // `close_reason` is `operator`, `confirmed`, `stale`, or `failed_start`
    // on a closed thread. `session_id` is the session of the thread agent,
    // archived when the agent ends. `reminded_at` is the time of the last
    // Inbox reminder of an untouched thread. Self-contained: when another
    // branch takes this number, move this entry to the end of the list.
    r#"
    ALTER TABLE threads ADD COLUMN close_proposal TEXT;
    ALTER TABLE threads ADD COLUMN close_reason TEXT;
    ALTER TABLE threads ADD COLUMN session_id TEXT;
    ALTER TABLE threads ADD COLUMN reminded_at TEXT;
    "#,
    // 79: the GitHub author gate (TER-85, `crate::store::github_gate`).
    // `projects.github_authors` is the JSON list of the GitHub logins
    // whose issues count as work; NULL: no gate. `questions.hold_card` is
    // the held card of a hold question that the daemon opened; NULL for
    // other questions. Self-contained: when another branch takes this
    // number, move this entry to the end of the list.
    r#"
    ALTER TABLE projects ADD COLUMN github_authors TEXT;
    ALTER TABLE questions ADD COLUMN hold_card TEXT;
    "#,
    // 80: labels on test runs and artifacts (TER-58). Versions are card
    // labels (`version:vnext`, `version:<X.Y.Z>`), so a run or an
    // artifact links to a label in place of a version object. A label is
    // free text. The version tables of migrations 3, 49, and 50
    // (`versions`, `version_cards`, `version_artifacts`, `version_reviews`,
    // `test_run_versions`) are no longer read or written, except that the
    // artifact prune still counts `version_artifacts`. They keep their
    // rows; a later version drops them. Self-contained: when another
    // branch takes this number, move this entry to the end of the list.
    r#"
    CREATE TABLE test_run_labels (
        run        INTEGER NOT NULL REFERENCES test_runs(pk) ON DELETE CASCADE,
        label      TEXT NOT NULL,
        created_by TEXT NOT NULL,
        created_at TEXT NOT NULL,
        PRIMARY KEY (run, label)
    );
    CREATE INDEX test_run_labels_label ON test_run_labels(label, run);
    CREATE TABLE artifact_labels (
        artifact   INTEGER NOT NULL REFERENCES artifacts(pk) ON DELETE CASCADE,
        label      TEXT NOT NULL,
        created_by TEXT NOT NULL,
        created_at TEXT NOT NULL,
        PRIMARY KEY (artifact, label)
    );
    CREATE INDEX artifact_labels_label ON artifact_labels(label, artifact);
    "#,
    // 81: the `run` grant level (TER-142, `crate::secrets::grants`). The
    // CHECK of `secret_grants.level` takes `run`, so the table is built
    // again. `targets` is the JSON array of the targets of a `run` grant;
    // NULL for `use` and `read`. No table refers to `secret_grants`.
    // Self-contained: when another branch takes this number, move this
    // entry to the end of the list.
    r#"
    CREATE TABLE secret_grants_v81 (
        secret_id  TEXT NOT NULL REFERENCES secrets(id) ON DELETE CASCADE,
        grantee    TEXT NOT NULL,
        level      TEXT NOT NULL CHECK (level IN ('use', 'run', 'read')),
        env        TEXT,
        created_by TEXT NOT NULL,
        created_at TEXT NOT NULL,
        targets    TEXT,
        PRIMARY KEY (secret_id, grantee)
    );
    INSERT INTO secret_grants_v81 (secret_id, grantee, level, env, created_by, created_at)
        SELECT secret_id, grantee, level, env, created_by, created_at FROM secret_grants;
    DROP TABLE secret_grants;
    ALTER TABLE secret_grants_v81 RENAME TO secret_grants;
    CREATE INDEX secret_grants_grantee ON secret_grants(grantee);
    "#,
    // 82: the column roles `validating`, `review`, `merge_queue`, and
    // `accepted` (TER-165, `crate::store::board`). The CHECK of
    // `board_columns.role` allows only `working` and `blocked`, and a CHECK
    // cannot change in place. So the column is replaced: add the new one,
    // copy, drop the index and the old column (its CHECK goes with it),
    // rename, and make the unique index again. The table is not rebuilt,
    // because `cards` refers to `board_columns`. The store checks the roles. Self-contained: when
    // another branch takes this number, move this entry to the end of the
    // list.
    r#"
    ALTER TABLE board_columns ADD COLUMN role_v82 TEXT;
    UPDATE board_columns SET role_v82 = role;
    DROP INDEX board_columns_role;
    ALTER TABLE board_columns DROP COLUMN role;
    ALTER TABLE board_columns RENAME COLUMN role_v82 TO role;
    CREATE UNIQUE INDEX board_columns_role ON board_columns(project, role)
        WHERE role IS NOT NULL;
    "#,
    // 83: the thread status `tracking` (TER-145, `crate::store::threads`):
    // the agent tracks handed-off work, parked, with no stale close.
    // `tracking_at` is the time the agent moved the thread into tracking;
    // it stays set while an operator post wakes the agent. The CHECK of
    // `threads.status` takes `tracking`, so the table is made again.
    // `thread_messages` refers to `threads` with ON DELETE CASCADE, so its
    // rows are kept in a plain copy over the drop and put back. The test
    // finds this entry by the name `threads_tracking`. Self-contained:
    // when another branch takes this number, move this entry to the end
    // of the list.
    r#"
    CREATE TABLE thread_messages_keep AS SELECT * FROM thread_messages ORDER BY rowid;
    DROP TABLE thread_messages;
    CREATE TABLE threads_tracking (
        id             TEXT PRIMARY KEY,
        topic          TEXT NOT NULL,
        scope          TEXT NOT NULL,
        template       TEXT,
        opened_by      TEXT NOT NULL,
        agent          TEXT,
        report_to      TEXT,
        status         TEXT NOT NULL
            CHECK (status IN ('waiting_operator', 'waiting_agent', 'tracking', 'closed')),
        response       TEXT,
        handoff        TEXT,
        created_at     TEXT NOT NULL,
        updated_at     TEXT NOT NULL,
        closed_at      TEXT,
        closed_by      TEXT,
        close_proposal TEXT,
        close_reason   TEXT,
        session_id     TEXT,
        reminded_at    TEXT,
        tracking_at    TEXT
    );
    INSERT INTO threads_tracking (id, topic, scope, template, opened_by, agent, report_to,
                                  status, response, handoff, created_at, updated_at,
                                  closed_at, closed_by, close_proposal, close_reason,
                                  session_id, reminded_at)
        SELECT id, topic, scope, template, opened_by, agent, report_to, status, response,
               handoff, created_at, updated_at, closed_at, closed_by, close_proposal,
               close_reason, session_id, reminded_at
        FROM threads;
    DROP TABLE threads;
    ALTER TABLE threads_tracking RENAME TO threads;
    CREATE INDEX threads_status ON threads(status, updated_at);
    CREATE INDEX threads_agent ON threads(agent);
    CREATE INDEX threads_report_to ON threads(report_to);
    CREATE TABLE thread_messages (
        id      TEXT PRIMARY KEY,
        thread  TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
        author  TEXT NOT NULL,
        kind    TEXT NOT NULL CHECK (kind IN ('message', 'response')),
        text    TEXT NOT NULL,
        at      TEXT NOT NULL
    );
    INSERT INTO thread_messages (id, thread, author, kind, text, at)
        SELECT id, thread, author, kind, text, at FROM thread_messages_keep ORDER BY rowid;
    DROP TABLE thread_messages_keep;
    CREATE INDEX thread_messages_thread ON thread_messages(thread, at);
    "#,
    // 84: the board rules of a project (TER-166, `crate::store::rules`):
    // the text of its rules.wcl, the SHA-256 of the text, the number of
    // rules, and `source` `api` or `file` (projects/<p>.rules.wcl).
    // Self-contained: when another branch takes this number, move this
    // entry to the end of the list.
    r#"
    CREATE TABLE project_rules (
        project    TEXT PRIMARY KEY REFERENCES projects(name) ON DELETE CASCADE,
        text       TEXT NOT NULL,
        hash       TEXT NOT NULL,
        rule_count INTEGER NOT NULL DEFAULT 0,
        source     TEXT NOT NULL DEFAULT 'api' CHECK (source IN ('api', 'file')),
        updated_by TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
    "#,
    // 85: the board rule engine (TER-168, `crate::rules_engine`): its own
    // event cursor, and `rule_runs`, both the action outbox (`pending`
    // rows, written in the same transaction as the cursor) and the run log
    // of each rule. `step` is the next action; `steps` the results of the
    // actions run. `lane` is the card (or `project:<p>`): the rows of one
    // lane run in order. `rule_json` is the rule as planned.
    // Self-contained: when another branch takes this number, move this
    // entry to the end of the list.
    r#"
    CREATE TABLE rule_cursor (
        id  INTEGER PRIMARY KEY CHECK (id = 1),
        seq INTEGER NOT NULL
    );
    CREATE TABLE rule_runs (
        id         TEXT PRIMARY KEY,
        project    TEXT NOT NULL REFERENCES projects(name) ON DELETE CASCADE,
        rule       TEXT NOT NULL,
        rule_hash  TEXT NOT NULL,
        ord        INTEGER NOT NULL,
        event_seq  INTEGER NOT NULL,
        card       TEXT,
        lane       TEXT NOT NULL,
        rule_json  TEXT NOT NULL CHECK (json_valid(rule_json)),
        cause      TEXT NOT NULL CHECK (json_valid(cause)),
        state      TEXT NOT NULL
                   CHECK (state IN ('pending', 'done', 'noop', 'failed', 'skipped')),
        step       INTEGER NOT NULL DEFAULT 0,
        steps      TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(steps)),
        reason     TEXT,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        UNIQUE (project, rule, event_seq, lane)
    );
    CREATE INDEX rule_runs_pending ON rule_runs(state, event_seq, ord);
    CREATE INDEX rule_runs_rule ON rule_runs(project, rule, created_at);
    "#,
    // 86: the cards that an Inbox thread tracks (TER-146, TER-144 D2,
    // `crate::store::threads::link_card`): a card of any project, linked
    // by the lead (`card new --inbox-thread`, `card link --inbox-thread`)
    // or by the thread agent (`thread track --card`). `project` is the
    // card's project at the link. A link gives no read access.
    // Self-contained: when another branch takes this number, move this
    // entry to the end of the list.
    r#"
    CREATE TABLE thread_cards (
        thread    TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
        project   TEXT NOT NULL,
        card      TEXT NOT NULL REFERENCES cards(id) ON DELETE CASCADE,
        linked_by TEXT NOT NULL,
        linked_at TEXT NOT NULL,
        PRIMARY KEY (thread, card)
    );
    CREATE INDEX thread_cards_card ON thread_cards(card);
    "#,
    // 87: who set the secret grants of a template (TER-88,
    // `crate::spawn_rights`). `agent_templates.grants_by` is the actor
    // label of the write that last set or changed `spec.grants`
    // (`operator`, `system` for a template file, or `agent:<id>`); NULL
    // when the template has no grants. A spawn or a create from the
    // template checks again that this actor may grant each secret.
    // Self-contained: when another branch takes this number, move this
    // entry to the end of the list.
    r#"
    ALTER TABLE agent_templates ADD COLUMN grants_by TEXT;
    "#,
    // 88: the planned duration of a job (TER-156, TER-155 D2).
    // `jobs.planned_secs` is the duration that the agent gave when it
    // queued the job (`--eta`); NULL: none. When the job starts, its
    // cards get `eta_at` = start + planned duration. A rerun and a split
    // keep it.
    // Self-contained: when another branch takes this number, move this
    // entry to the end of the list.
    r#"
    ALTER TABLE jobs ADD COLUMN planned_secs INTEGER;
    "#,
    // 89: the slot of a running job (TER-190, `crate::jobs`). `jobs.slot`
    // is `1` to the project's `slots` for a validate job and `0` for a
    // merge; NULL for a queued job and for a job started before this
    // migration. Each slot has one persistent work tree on the target.
    // A cancelled job keeps its slot, so its tree stays taken while its
    // processes drain. Self-contained: when another branch takes this
    // number, move this entry to the end of the list.
    r#"
    ALTER TABLE jobs ADD COLUMN slot INTEGER;
    "#,
    // 90: the slots whose tree is made new before the next job (TER-190
    // review, finding 2, `crate::jobs`): the kill of a lost job did not
    // confirm that its processes ended. `job` is that lost job. The receive
    // step of the next job of the slot wipes the tree and deletes the row.
    // Self-contained: when another branch takes this number, move this
    // entry to the end of the list.
    r#"
    CREATE TABLE job_slot_wipes (
        project TEXT NOT NULL,
        slot INTEGER NOT NULL,
        job TEXT NOT NULL,
        marked_at TEXT NOT NULL,
        PRIMARY KEY (project, slot)
    );
    "#,
    // 91: who runs the card flow of a project (TER-170,
    // `crate::store::projects::board_flow`). `projects.board_flow` is
    // `rules` (the board rules with the built-in pack) or NULL (`code`,
    // the daemon's code). `board_flow_at` is the time of the last switch:
    // an older event starts no run of the built-in pack.
    // Self-contained: when another branch takes this number, move this
    // entry to the end of the list.
    r#"
    ALTER TABLE projects ADD COLUMN board_flow TEXT;
    ALTER TABLE projects ADD COLUMN board_flow_at TEXT;
    "#,
    // 92: retain the origin of custom bound-agent return turns (TER-217).
    // Self-contained: move this entry to the end if another branch takes its number.
    r#"
    ALTER TABLE queued_turns ADD COLUMN unpark_card TEXT;
    "#,
    // 93: Sidebar bookmarks, scoped to the operator, independent of projects.
    r#"
    CREATE TABLE bookmarks (
        owner TEXT NOT NULL,
        href TEXT NOT NULL,
        title TEXT NOT NULL,
        PRIMARY KEY (owner, href)
    );
    "#,
    // 94: Board groups (TER-180). Self-contained, append after concurrent migrations.
    r#"
    ALTER TABLE cards ADD COLUMN kind TEXT NOT NULL DEFAULT 'card'
        CHECK(kind IN ('card', 'group'));
    CREATE TABLE card_groups (
        card TEXT PRIMARY KEY REFERENCES cards(id) ON DELETE CASCADE,
        project TEXT NOT NULL REFERENCES projects(name) ON DELETE CASCADE,
        key TEXT NOT NULL,
        rule TEXT NOT NULL,
        done_when TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT '',
        eta_jobs INTEGER NOT NULL DEFAULT 0,
        close_at TEXT,
        closed_at TEXT,
        UNIQUE(project, key)
    );
    CREATE TABLE group_members (
        group_card TEXT NOT NULL REFERENCES card_groups(card) ON DELETE CASCADE,
        card TEXT NOT NULL REFERENCES cards(id) ON DELETE CASCADE,
        position INTEGER NOT NULL,
        PRIMARY KEY(group_card, card)
    );
    CREATE INDEX group_members_card ON group_members(card);
    CREATE TABLE group_steps (
        group_card TEXT NOT NULL REFERENCES card_groups(card) ON DELETE CASCADE,
        name TEXT NOT NULL,
        position INTEGER NOT NULL,
        state TEXT NOT NULL CHECK(state IN ('pending', 'running', 'done', 'failed')),
        text TEXT NOT NULL DEFAULT '',
        eta_at TEXT,
        PRIMARY KEY(group_card, name)
    );
    "#,
    // 95: Operator preferences (TER-253). Self-contained migration.
    r#"
    CREATE TABLE operator_preferences (
        owner TEXT PRIMARY KEY,
        default_agent TEXT REFERENCES agents(id) ON DELETE SET NULL
    );
    "#,
    // 96: built-in human card comment delivery (TER-254).
    r#"
    ALTER TABLE projects ADD COLUMN human_comment_notifications INTEGER NOT NULL DEFAULT 1;
    "#,
    // 97: Card wakes held while an Inbox thread waits on the operator (TER-219).
    r#"
    CREATE TABLE thread_pending_wakes (
        thread TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
        reason TEXT NOT NULL CHECK(reason IN ('blocked', 'review')),
        PRIMARY KEY(thread, reason)
    );
    "#,
];

thread_local! {
    static HELD_DATABASES: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

struct DatabaseScope;

impl DatabaseScope {
    fn enter(conn: &Arc<Mutex<Connection>>) -> Self {
        HELD_DATABASES.with(|held| held.borrow_mut().push(Arc::as_ptr(conn) as usize));
        Self
    }
}

impl Drop for DatabaseScope {
    fn drop(&mut self) {
        HELD_DATABASES.with(|held| {
            held.borrow_mut().pop();
        });
    }
}

/// The shared database handle. One connection behind a mutex; async calls
/// run on the blocking pool. Synchronous journal calls document their locks.
#[derive(Clone)]
pub struct Db {
    project_pages_v2: bool,
    conn: Arc<Mutex<Connection>>,
    hub: EventHub,
    /// The redaction matcher (PLAN §16b.6). Each write transaction takes a
    /// snapshot of it under the database lock ([`Tx::redact`]). A write
    /// that adds a value builds the new matcher before its commit and swaps
    /// it in before it lets the lock go ([`Tx::rebuild_redactor`]), so no
    /// other write sees the value with the old matcher.
    redactor: RedactorHandle,
    /// Serializes the host pin resolution of this database
    /// ([`crate::jobs::prepare_hosts`]). Per database, not per process: the
    /// test binary runs hundreds of daemons in one process, and one shared
    /// lock queued every boot behind the DNS lookups of the others (TER-648).
    host_pins: Arc<tokio::sync::Mutex<crate::jobs::HostRetries>>,
}

impl Db {
    pub fn new(conn: Connection, hub: EventHub) -> Self {
        Self {
            project_pages_v2: false,
            conn: Arc::new(Mutex::new(conn)),
            hub,
            redactor: RedactorHandle::default(),
            host_pins: Arc::default(),
        }
    }

    /// Carry the server capability into every compatibility writer.
    pub fn with_project_pages_v2(mut self, enabled: bool) -> Self {
        self.project_pages_v2 = enabled;
        self
    }

    /// The lock of [`crate::jobs::prepare_hosts`] on this database.
    pub(crate) fn host_pins(&self) -> &tokio::sync::Mutex<crate::jobs::HostRetries> {
        &self.host_pins
    }

    pub fn hub(&self) -> &EventHub {
        &self.hub
    }

    /// The daemon's redaction matcher. Clones share the same slot.
    pub fn redactor(&self) -> &RedactorHandle {
        &self.redactor
    }

    /// Run a read on the blocking pool.
    #[track_caller]
    pub fn read<T, F>(&self, f: F) -> impl std::future::Future<Output = ApiResult<T>> + '_
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> ApiResult<T> + Send + 'static,
    {
        let conn = self.conn.clone();
        let queued = tracing::enabled!(target: "terrariumd::db_timing", tracing::Level::DEBUG)
            .then(std::time::Instant::now);
        let caller = std::panic::Location::caller();
        async move {
            crate::links::spawn_blocking(move || {
                let started = queued.map(|_| std::time::Instant::now());
                let guard = match conn.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                let acquired = queued.map(|_| std::time::Instant::now());
                let _scope = DatabaseScope::enter(&conn);
                let result = f(&guard);
                let finished = queued.map(|_| std::time::Instant::now());
                drop(guard);
                if let (Some(queued), Some(started), Some(acquired), Some(finished)) =
                    (queued, started, acquired, finished)
                {
                    tracing::debug!(target: "terrariumd::db_timing", operation = "read",
                    caller_file = caller.file(),
                    caller_line = caller.line(),
                    scheduling_us = started.duration_since(queued).as_micros() as u64,
                    mutex_wait_us = acquired.duration_since(started).as_micros() as u64,
                    sql_us = finished.duration_since(acquired).as_micros() as u64,
                    "database access");
                }
                result
            })
            .await
            .map_err(|e| ApiError::internal(format!("database task failed: {e}")))?
        }
    }

    /// Run a read on the calling thread. Only for a short read at the
    /// daemon start, where the caller cannot wait on the blocking pool.
    pub fn read_now<T>(&self, f: impl FnOnce(&Connection) -> ApiResult<T>) -> ApiResult<T> {
        let guard = match self.conn.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _scope = DatabaseScope::enter(&self.conn);
        f(&guard)
    }

    /// Journal calls may hold the host registry lock. Never wait for the
    /// writer here: a write can itself need registry state. Callers retain
    /// uncommitted events and retry ResourceBusy outside both locks.
    pub(crate) fn journal_now<T>(
        &self,
        wait: bool,
        f: impl FnOnce(&mut Connection) -> ApiResult<T>,
    ) -> ApiResult<T> {
        if wait
            && HELD_DATABASES
                .with(|held| held.borrow().contains(&(Arc::as_ptr(&self.conn) as usize)))
        {
            return Err(ApiError::internal(
                "journal cannot wait inside a database closure",
            ));
        }
        let mut guard = if wait {
            match self.conn.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            }
        } else {
            match self.conn.try_lock() {
                Ok(g) => g,
                Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    return Err(ApiError::new(
                        crate::error::Code::ResourceBusy,
                        "host journal writer is busy",
                    ));
                }
            }
        };
        let _scope = DatabaseScope::enter(&self.conn);
        // Journal ACKs promise durable output, including across power loss.
        guard.pragma_update(None, "synchronous", "FULL")?;
        f(&mut guard)
    }

    /// Run a write in one transaction. `actor` labels the events and audit
    /// rows that the write makes. The transaction commits when `f` returns
    /// `Ok`; then the new events and audit rows go to the subscribers.
    #[track_caller]
    pub fn write<T, F>(
        &self,
        actor: impl Into<String>,
        f: F,
    ) -> impl std::future::Future<Output = ApiResult<T>> + '_
    where
        T: Send + 'static,
        F: for<'c> FnOnce(&mut Tx<'c>) -> ApiResult<T> + Send + 'static,
    {
        self.write_with_durability(actor, f, false)
    }

    /// Security intents must survive power loss before a helper may act.
    /// FULL is set under the connection lock, before BEGIN IMMEDIATE.
    #[track_caller]
    pub fn write_full<T, F>(
        &self,
        actor: impl Into<String>,
        f: F,
    ) -> impl std::future::Future<Output = ApiResult<T>> + '_
    where
        T: Send + 'static,
        F: for<'c> FnOnce(&mut Tx<'c>) -> ApiResult<T> + Send + 'static,
    {
        self.write_with_durability(actor, f, true)
    }

    #[track_caller]
    fn write_with_durability<T, F>(
        &self,
        actor: impl Into<String>,
        f: F,
        full: bool,
    ) -> impl std::future::Future<Output = ApiResult<T>> + '_
    where
        T: Send + 'static,
        F: for<'c> FnOnce(&mut Tx<'c>) -> ApiResult<T> + Send + 'static,
    {
        let conn = self.conn.clone();
        let actor = actor.into();
        let project_pages_v2 = self.project_pages_v2;
        // The link of the request (TER-61 review round 5, F9): the
        // task-local is gone inside the blocking task, so take it here. An
        // actor label `link:<id>` counts too.
        let link = crate::links::current_link_id()
            .or_else(|| actor.strip_prefix("link:").map(str::to_owned));
        let handle = self.redactor.clone();
        let queued = tracing::enabled!(target: "terrariumd::db_timing", tracing::Level::DEBUG)
            .then(std::time::Instant::now);
        // Capture the call site before constructing the future. An async fn's
        // track_caller would identify the executor, not the write's caller.
        // Source locations are static labels, never actor names or SQL.
        let caller = if queued.is_some() {
            Some(std::panic::Location::caller())
        } else {
            None
        };
        async move {
            let (value, events, audits) = crate::links::spawn_blocking(move || {
                let started = queued.map(|_| std::time::Instant::now());
                let mut guard = match conn.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                let acquired = queued.map(|_| std::time::Instant::now());
                let _scope = DatabaseScope::enter(&conn);
                // The snapshot comes after the lock: a write that changed a
                // secret swapped its matcher in before it let the lock go.
                let result = (|| {
                    if full {
                        guard.pragma_update(None, "synchronous", "FULL")?;
                    }
                    let redactor = handle.get();
                    // IMMEDIATE: the write lock comes at the start, so a write of
                    // another connection to the same file (a second daemon on the
                    // same state dir, for example a test that starts the daemon
                    // again) makes this one wait up to the busy timeout. A DEFERRED
                    // transaction that reads first gets `database is locked` at
                    // once when the other connection commits between its read and
                    // its first write; SQLite does not wait in that case.
                    let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    let mut t = Tx {
                        project_pages_v2,
                        tx,
                        actor,
                        link,
                        cause: None,
                        events: Vec::new(),
                        audits: RefCell::default(),
                        redactor,
                        view: OnceCell::new(),
                        rebuild: None,
                        files: Vec::new(),
                    };
                    let value = f(&mut t)?;
                    let Tx {
                        tx,
                        events,
                        audits,
                        rebuild,
                        files,
                        ..
                    } = t;
                    // The new matcher reads the store as this transaction left it.
                    // A failure to build it fails the write (PLAN §16b.6, S11).
                    let next = match rebuild {
                        Some(dir) => Some(crate::secrets::redact::build(&tx, &dir)?),
                        None => None,
                    };
                    // The staged files go to disk before the commit; a failure
                    // puts the old files back, and the transaction rolls back.
                    let undo = apply_files(&files)?;
                    if let Err(e) = tx.commit() {
                        undo_files(undo);
                        return Err(e.into());
                    }
                    // The swap comes before the guard goes (at the end of this
                    // closure): the next write takes its snapshot after it.
                    if let Some(r) = next {
                        tracing::debug!(patterns = r.pattern_count(), "redaction matcher built");
                        handle.replace(r);
                    }
                    Ok::<_, ApiError>((value, events, audits.into_inner()))
                })();
                let finished = queued.map(|_| std::time::Instant::now());
                drop(guard);
                if let (Some(queued), Some(started), Some(acquired), Some(finished), Some(caller)) =
                    (queued, started, acquired, finished, caller)
                {
                    tracing::debug!(target: "terrariumd::db_timing", operation = "write",
                    caller_file = caller.file(),
                    caller_line = caller.line(),
                    success = result.is_ok(),
                    scheduling_us = started.duration_since(queued).as_micros() as u64,
                    mutex_wait_us = acquired.duration_since(started).as_micros() as u64,
                    sql_us = finished.duration_since(acquired).as_micros() as u64,
                    "database access");
                }
                result
            })
            .await
            .map_err(|e| ApiError::internal(format!("database task failed: {e}")))??;
            for e in events {
                self.hub.publish(e);
            }
            for a in audits {
                self.hub.publish_audit(a);
            }
            Ok(value)
        }
    }
}

/// A write transaction with the actor and the events and audit rows it made.
pub struct Tx<'c> {
    pub project_pages_v2: bool,
    tx: rusqlite::Transaction<'c>,
    /// `operator`, `agent:<ulid>`, `system`, or `schedule:<ulid>` (the
    /// writes of a trigger run).
    pub actor: String,
    /// The link (`link:<id>`, TER-61) whose request made this write, or
    /// `None`. `store::mail::send` refuses a message of a link write to a
    /// sysadmin agent.
    pub link: Option<String>,
    /// When set, every event of the transaction gets it as `data.cause`
    /// (a trigger run: `{schedule, run, event, depth}`).
    pub cause: Option<serde_json::Value>,
    events: Vec<StoredEvent>,
    /// The audit rows that the transaction wrote or changed, for the live
    /// streams after the commit (`audit` frames).
    audits: RefCell<Vec<AuditEntry>>,
    /// The redaction matcher at the start of the transaction.
    redactor: Arc<Redactor>,
    /// The labels that the writes of this actor show ([`writer_view`]).
    view: OnceCell<View>,
    /// Set by [`Self::rebuild_redactor`]: the state dir of the build.
    rebuild: Option<PathBuf>,
    /// The file writes of [`Self::stage_file`], in order.
    files: Vec<FileOp>,
}

/// A file write that a transaction stages ([`Tx::stage_file`]).
#[derive(Debug, Clone)]
struct FileOp {
    path: PathBuf,
    /// The new content. `None` removes the file.
    content: Option<Vec<u8>>,
}

/// The old state of a file that [`apply_files`] changed.
type Undo = Vec<(PathBuf, Option<Vec<u8>>)>;

/// Write the staged files, each one atomically. On a failure, put back
/// the files that changed before it.
fn apply_files(files: &[FileOp]) -> ApiResult<Undo> {
    let mut undo: Undo = Vec::new();
    for op in files {
        let before = std::fs::read(&op.path).ok();
        let result = match &op.content {
            Some(bytes) => write_file(&op.path, bytes),
            None => match std::fs::remove_file(&op.path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            },
        };
        if let Err(e) = result {
            undo_files(undo);
            return Err(ApiError::internal(format!("{}: {e}", op.path.display())));
        }
        undo.push((op.path.clone(), before));
    }
    Ok(undo)
}

/// Write `bytes` to a temp file next to `path`, then rename it.
fn write_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.tmp-{}", ulid::Ulid::new()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn undo_files(undo: Undo) {
    for (path, before) in undo.into_iter().rev() {
        let r = match before {
            Some(bytes) => write_file(&path, &bytes),
            None => std::fs::remove_file(&path),
        };
        if let Err(e) = r {
            tracing::warn!(path = %path.display(), "cannot put the file back: {e}");
        }
    }
}

impl<'c> std::ops::Deref for Tx<'c> {
    type Target = rusqlite::Transaction<'c>;
    fn deref(&self) -> &Self::Target {
        &self.tx
    }
}

impl<'c> Tx<'c> {
    /// A transaction for unit tests: the events stay in it.
    #[cfg(test)]
    pub fn for_test(tx: rusqlite::Transaction<'c>, actor: &str) -> Self {
        Self {
            project_pages_v2: false,
            tx,
            actor: actor.to_owned(),
            link: actor.strip_prefix("link:").map(str::to_owned),
            cause: None,
            events: Vec::new(),
            audits: RefCell::default(),
            redactor: Arc::new(Redactor::empty()),
            view: OnceCell::new(),
            rebuild: None,
            files: Vec::new(),
        }
    }

    /// Commit a test transaction (the events stay in it).
    #[cfg(test)]
    pub fn commit_for_test(self) -> rusqlite::Result<()> {
        self.tx.commit()
    }

    /// Write the staged files of a test transaction now.
    #[cfg(test)]
    pub fn apply_files_for_test(&mut self) -> ApiResult<()> {
        apply_files(&std::mem::take(&mut self.files)).map(|_| ())
    }

    /// Write `content` to `path` (`None`: remove the file) when the
    /// transaction commits: just before the commit, so that a failure of
    /// the write rolls the transaction back, and a failure of the commit
    /// puts the old files back. A write that fails before its end writes
    /// no file.
    pub fn stage_file(&mut self, path: PathBuf, content: Option<Vec<u8>>) {
        self.files.push(FileOp { path, content });
    }

    /// Build the redaction matcher again in this transaction, before its
    /// commit, and swap it in before the lock goes (PLAN §16b.6, S11).
    /// Each write that adds or changes a secret value calls it.
    pub fn rebuild_redactor(&mut self, state_dir: &Path) {
        self.rebuild = Some(state_dir.to_owned());
    }

    /// The labels that the writes of this transaction show.
    fn view(&self) -> &View {
        self.view.get_or_init(|| writer_view(&self.tx, &self.actor))
    }

    /// Replace each known secret in `s` with `[secret:<label>]` (PLAN
    /// §16b.6). Borrows `s` when nothing matches. Each store function that
    /// writes a free text (a mail body, a card, a wiki page) calls it on the
    /// text before the write, so the database never holds the value. The
    /// label shows a name only when the actor may see it ([`writer_view`]).
    pub fn redact<'a>(&self, s: &'a str) -> Cow<'a, str> {
        if self.redactor.is_empty() {
            return Cow::Borrowed(s);
        }
        self.redactor.redact_str_as(s, self.view()).0
    }

    /// [`Self::redact`] of an optional text.
    pub fn redact_opt<'a>(&self, s: Option<&'a str>) -> Option<Cow<'a, str>> {
        s.map(|s| self.redact(s))
    }

    /// [`Self::redact`] of an owned text: the same allocation when nothing
    /// matches.
    pub fn redact_string(&self, s: String) -> String {
        if self.redactor.is_empty() {
            return s;
        }
        self.redactor.redact_string_as(s, self.view()).0
    }

    /// [`Self::redact`] of each string value in a JSON value
    /// ([`redact_json_as`]).
    pub fn redact_json(&self, v: serde_json::Value) -> serde_json::Value {
        self.redact_json_counted(v).0
    }

    /// [`Self::redact`] of each string value of a JSON value, under every
    /// key: the values of a DB record are free text, whatever the field
    /// name ([`redact_json_values`]).
    pub fn redact_values(&self, v: serde_json::Value) -> serde_json::Value {
        if self.redactor.is_empty() {
            return v;
        }
        redact_json_values(&self.redactor, self.view(), v)
    }

    /// [`Self::redact_json`] with the number of matches.
    fn redact_json_counted(&self, v: serde_json::Value) -> (serde_json::Value, usize) {
        if self.redactor.is_empty() {
            return (v, 0);
        }
        redact_json_as(&self.redactor, self.view(), v)
    }

    /// The redacted detail of an audit row. With a match, an object gets
    /// `"redacted": <the number of matches>` (PLAN §16b.6).
    fn audit_detail(&self, detail: serde_json::Value) -> serde_json::Value {
        let (mut detail, n) = self.redact_json_counted(detail);
        if n > 0
            && let serde_json::Value::Object(m) = &mut detail
        {
            m.insert("redacted".to_owned(), n.into());
        }
        detail
    }

    /// Run `f` in a SQLite savepoint. On an error of `f`, its writes roll
    /// back, the events, audit rows and staged files that it added are
    /// dropped, a redactor rebuild that it asked for is undone, and the
    /// rest of the transaction stays: all of `f` or none of it. The error
    /// goes to the caller.
    pub fn savepoint<T>(&mut self, f: impl FnOnce(&mut Self) -> ApiResult<T>) -> ApiResult<T> {
        let marks = (
            self.events.len(),
            self.audits.borrow().len(),
            self.files.len(),
        );
        let rebuild = self.rebuild.clone();
        self.tx.execute_batch("SAVEPOINT tx_savepoint")?;
        match f(self) {
            Ok(v) => {
                self.tx.execute_batch("RELEASE tx_savepoint")?;
                Ok(v)
            }
            Err(e) => {
                self.tx
                    .execute_batch("ROLLBACK TO tx_savepoint; RELEASE tx_savepoint")?;
                self.events.truncate(marks.0);
                self.audits.borrow_mut().truncate(marks.1);
                self.files.truncate(marks.2);
                self.rebuild = rebuild;
                Err(e)
            }
        }
    }

    /// Append an event to the log. Subscribers get it after the commit.
    /// The summary and the data are redacted first, so the event log, the
    /// streams, and the trigger payloads hold no known secret.
    pub fn emit(&mut self, mut draft: EventDraft) -> ApiResult<StoredEvent> {
        if let (Some(c), serde_json::Value::Object(m)) = (&self.cause, &mut draft.data) {
            m.entry("cause").or_insert_with(|| c.clone());
        }
        if !self.redactor.is_empty() {
            draft.summary = self.redact_string(std::mem::take(&mut draft.summary));
            draft.data = self.redact_json(std::mem::take(&mut draft.data));
        }
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
        self.audit_with_id(
            &ulid::Ulid::new().to_string(),
            action,
            target,
            project,
            detail,
        )
    }

    /// Change the `detail` of the audit row `id` (an aggregate row, such
    /// as the `auth.login_failed` count of one address and window).
    pub fn audit_update(&self, id: &str, detail: serde_json::Value) -> ApiResult<()> {
        let detail = self.audit_detail(detail);
        self.tx.execute(
            "UPDATE audit SET detail = ?2, at = ?3 WHERE id = ?1",
            rusqlite::params![id, detail.to_string(), now()],
        )?;
        if let Some(mut row) = crate::store::audit::get(&self.tx, id)? {
            crate::store::audit::attach_links(&self.tx, std::slice::from_mut(&mut row))?;
            self.audits.borrow_mut().push(row);
        }
        Ok(())
    }

    /// [`Self::audit`] with the row ID given (a ULID), so that a later
    /// [`Self::audit_update`] can find the row.
    pub fn audit_with_id(
        &self,
        id: &str,
        action: &str,
        target: &str,
        project: Option<&str>,
        detail: serde_json::Value,
    ) -> ApiResult<()> {
        let detail = self.audit_detail(detail);
        let at = now();
        self.tx.execute(
            "INSERT INTO audit (id, actor, action, target, project, detail, at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                id,
                self.actor,
                action,
                target,
                project,
                detail.to_string(),
                at
            ],
        )?;
        // `seq` is the rowid (INTEGER PRIMARY KEY).
        let mut row = AuditEntry {
            id: id.to_owned(),
            seq: self.tx.last_insert_rowid(),
            at,
            actor: self.actor.clone(),
            action: action.to_owned(),
            target: target.to_owned(),
            project: project.map(str::to_owned),
            detail,
            link: Default::default(),
            link_label: None,
        };
        // The live `audit` frame carries the session too.
        crate::store::audit::attach_links(&self.tx, std::slice::from_mut(&mut row))?;
        self.audits.borrow_mut().push(row);
        Ok(())
    }
}

/// The labels that the records of `actor` show (PLAN §16b.6, S5): each
/// name for the operator and a sysadmin agent; for another agent, the
/// names of its own project, and `[secret]` for the others (global, other
/// projects); `[secret]` for each other actor (`system`, a trigger, an
/// event source), whose text can come from any agent.
pub fn writer_view(conn: &Connection, actor: &str) -> View {
    if actor == "operator" {
        return View::All;
    }
    let Some(id) = actor.strip_prefix("agent:") else {
        return View::Hidden;
    };
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT project, roles FROM agents WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    match row {
        Some((project, roles)) => {
            let roles: Vec<terrarium_core::Role> = crate::store::json_col(&roles);
            if roles.contains(&terrarium_core::Role::Sysadmin) {
                View::All
            } else {
                View::Scope(project)
            }
        }
        None => View::Hidden,
    }
}

/// The keys whose string values are identifiers, not free text: an ID, an
/// actor, an agent, a project, a kind, a grantee, a scope, a name. The
/// redaction leaves them as they are (PLAN §16b.6, S4), as it leaves each
/// object key. A value that looks like an identifier is not a secret
/// value: the store refuses it, or leaves it out of the matcher
/// (`crate::secrets::redact`).
const IDENTIFIER_KEYS: &[&str] = &[
    "id",
    "ids",
    "actor",
    "agent",
    "agents",
    "project",
    "projects",
    "kind",
    "grantee",
    "grantee_label",
    "scope",
    "name",
    "by",
    "created_by",
    "from",
    "to",
    "level",
    "action",
    "target",
    "schedule",
    "run",
    "event",
    "session",
    "card",
    "thread",
    "subscription",
    "harness",
    "status",
    "state",
];

/// True for a key whose string values are identifiers: one of
/// [`IDENTIFIER_KEYS`], or a key that ends in `_id`, `_ids`, or `_name`.
fn identifier_key(k: &str) -> bool {
    IDENTIFIER_KEYS.contains(&k)
        || k.ends_with("_id")
        || k.ends_with("_ids")
        || k.ends_with("_name")
}

/// Redact each string value of a JSON value, each label in full (PLAN
/// §16b.6). Object keys and the values of identifier keys stay
/// ([`redact_json_as`]). Returns the value as it is when the matcher is
/// empty.
pub fn redact_json(r: &Redactor, v: serde_json::Value) -> serde_json::Value {
    redact_json_as(r, &View::All, v).0
}

/// [`redact_json`] for a view, with the number of matches. The keys of an
/// object stay as they are: a key is a name of the record, not a value. A
/// string (or a list of strings) under an identifier key stays too, so a
/// secret value cannot change the IDs, actors, and names of a record.
pub fn redact_json_as(
    r: &Redactor,
    view: &View,
    v: serde_json::Value,
) -> (serde_json::Value, usize) {
    if r.is_empty() {
        return (v, 0);
    }
    let mut n = 0;
    let v = redact_value(r, view, v, false, true, &mut n);
    (v, n)
}

/// Redact each string value of a JSON value that an agent or a harness
/// wrote (a tool input of a transcript): the keys stay, and each string
/// value is free text, under an identifier key too.
pub fn redact_json_values(r: &Redactor, view: &View, v: serde_json::Value) -> serde_json::Value {
    if r.is_empty() {
        return v;
    }
    let mut n = 0;
    redact_value(r, view, v, false, false, &mut n)
}

fn redact_value(
    r: &Redactor,
    view: &View,
    v: serde_json::Value,
    identifier: bool,
    ids: bool,
    n: &mut usize,
) -> serde_json::Value {
    use serde_json::Value;
    match v {
        Value::String(s) if identifier => Value::String(s),
        Value::String(s) => {
            let (s, k) = r.redact_string_as(s, view);
            *n += k;
            Value::String(s)
        }
        Value::Array(a) => Value::Array(
            a.into_iter()
                .map(|v| redact_value(r, view, v, identifier, ids, n))
                .collect(),
        ),
        Value::Object(m) => Value::Object(
            m.into_iter()
                .map(|(k, v)| {
                    let id = ids && identifier_key(&k);
                    let v = redact_value(r, view, v, id, ids, n);
                    (k, v)
                })
                .collect(),
        ),
        v => v,
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
    // The busy timeout comes first: the pragmas take locks too. Without it,
    // an open while another connection to the file holds the exclusive lock
    // fails at once with `database is locked`. The last connection of a WAL
    // database takes that lock when it closes, to checkpoint (TER-646: a
    // test daemon started again while the old daemon's connection closed).
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    // Defence in depth for permanent history: REPLACE's implicit DELETE must
    // run delete guards too. Live-workspace insert guards also work without it.
    conn.pragma_update(None, "recursive_triggers", true)?;
    migrate(&mut conn)?;
    Ok(conn)
}

/// Run pending legacy migrations, then named migrations in filename order.
pub fn migrate(conn: &mut Connection) -> anyhow::Result<usize> {
    let legacy = migrate_with(conn, MIGRATIONS)?;
    Ok(legacy + migrate_named(conn, NAMED_MIGRATIONS)?)
}

fn migrate_named(conn: &mut Connection, migrations: &[(&str, &str)]) -> anyhow::Result<usize> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            name TEXT PRIMARY KEY NOT NULL,
            applied_at TEXT NOT NULL
        );",
    )?;
    let mut applied = 0;
    for (name, sql) in migrations {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE name = ?1)",
            [name],
            |row| row.get(0),
        )?;
        if exists {
            continue;
        }
        tx.execute_batch(sql)
            .with_context(|| format!("migration {name} failed"))?;
        if *name == "ter-970-seeded-waits.sql" {
            complete_seeded_waits(&tx)?;
        }
        tx.execute(
            "INSERT INTO schema_migrations (name, applied_at) VALUES (?1, ?2)",
            rusqlite::params![name, now()],
        )?;
        tx.commit()?;
        tracing::info!(name, "applied migration");
        applied += 1;
    }
    Ok(applied)
}

// Use the same view as board::unmet, without publishing events or running actions.
// Only the legacy seed shape is eligible. Later set() generations are untouched.
fn complete_seeded_waits(conn: &Connection) -> anyhow::Result<()> {
    let mut after = String::new();
    loop {
        let page = conn
            .prepare("SELECT card FROM card_waits WHERE card > ?1 ORDER BY card LIMIT 128")?
            .query_map([&after], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if page.is_empty() {
            break;
        }
        for card in &page {
            conn.execute(
                "UPDATE card_waits SET met_at=?2
                 WHERE card=?1 AND met_at IS NULL AND mode='all'
                   AND action='{}' AND setter='operator'
                   AND NOT EXISTS (SELECT 1 FROM card_dependency_unmet WHERE card=?1)",
                rusqlite::params![card, now()],
            )?;
        }
        after = page.last().cloned().unwrap_or_default();
    }
    Ok(())
}

fn migrate_with(conn: &mut Connection, migrations: &[&str]) -> anyhow::Result<usize> {
    let current: usize = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    // A fresh database has no state to preserve between versions. Commit
    // its schema once, without changing SQLite's durability settings.
    if current == 0 {
        let tx = conn.transaction()?;
        for (index, sql) in migrations.iter().enumerate() {
            let version = index + 1;
            tx.execute_batch(sql)
                .with_context(|| format!("migration {version} failed"))?;
            tx.pragma_update(None, "user_version", version)?;
        }
        tx.commit()?;
        return Ok(migrations.len());
    }
    let mut applied = 0;
    for (index, sql) in migrations.iter().enumerate().skip(current) {
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
    use crate::store::board::{BLOCKED, BLOCKED_INFO_SQL, Card, LAST_MOVED_AT_SQL};
    use crate::store::card_history::RECENT_PROJECT_MOVES_SQL;

    fn migration_count() -> usize {
        MIGRATIONS.len() + NAMED_MIGRATIONS.len()
    }

    #[test]
    fn rule_selection_is_column_indexed_and_does_not_render_board_history() {
        use crate::store::board::{SELECTION_SQL, get_board, selection_cards};
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        conn.execute_batch(
            "INSERT INTO projects(name,created_at,updated_at) VALUES ('p','t','t');
             INSERT INTO boards(project,created_at) VALUES ('p','t');
             INSERT INTO board_columns(id,project,name,position) VALUES
               (1,'p','accepted',0),(2,'p','done',1);
             WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<2000)
             INSERT INTO cards(id,project,number,column_id,position,title,labels,created_by,created_at,updated_at)
               SELECT 'P-'||x,'p',x,CASE WHEN x<=3 THEN 1 ELSE 2 END,x,'fixture',
                 '[\"version:vnext\"]','operator','2026-11-12T00:00:00Z','t' FROM n;
             INSERT INTO card_comments(id,card,author,body,created_at)
               SELECT 'comment-'||id,id,'operator',hex(zeroblob(1000)),'t' FROM cards;
             UPDATE cards SET archived=1 WHERE id='P-2';
             UPDATE cards SET kind='group' WHERE id='P-3';",
        ).unwrap();
        let start = std::time::Instant::now();
        let old = get_board(&conn, "p").unwrap();
        let before = start.elapsed();
        let start = std::time::Instant::now();
        let selected = selection_cards(&conn, "p", "accepted").unwrap();
        let after = start.elapsed();
        eprintln!("2000 cards, 2000 2KB comments: full board {before:?}, rule selection {after:?}");
        let expected: Vec<_> = old
            .cards
            .iter()
            .filter(|c| c.column == "accepted" && c.kind != "group")
            .map(|c| (&c.id, &c.labels, &c.created_at))
            .collect();
        let actual: Vec<_> = selected
            .iter()
            .map(|c| (&c.id, &c.labels, &c.created_at))
            .collect();
        assert_eq!(actual, expected);
        assert_eq!(selected.len(), 1);
        assert!(selected[0].comments.is_empty());
        let pick = crate::store::version_groups::pick(&conn, "p").unwrap();
        assert_eq!(pick.waiting, 1);
        assert_eq!(pick.cards[0].id, "P-1");
        assert!(selection_cards(&conn, "p", "missing").unwrap().is_empty());
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {SELECTION_SQL}"))
            .unwrap()
            .query_map(["p", "accepted"], |r| r.get(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            plan.iter()
                .any(|p| p.contains("SEARCH c USING INDEX cards_column")),
            "{plan:?}"
        );
        assert!(
            !plan
                .iter()
                .any(|p| p.contains("SCAN c") || p.contains("TEMP B-TREE")),
            "{plan:?}"
        );
    }

    #[tokio::test]
    async fn failed_profiled_write_rolls_back_and_releases_lock() {
        let db = Db::new(test_db(), EventHub::default());
        let result = db
            .write("system", |t| {
                t.execute_batch("CREATE TABLE failed_profile_write (id INTEGER)")?;
                Err::<(), _>(ApiError::internal("intentional rollback"))
            })
            .await;
        assert!(result.is_err());
        db.write("system", |t| {
            t.execute_batch("CREATE TABLE failed_profile_write (id INTEGER)")?;
            Ok(())
        })
        .await
        .unwrap();
    }

    #[test]
    fn legacy_migration_list_is_frozen() {
        assert_eq!(
            MIGRATIONS.len(),
            97,
            "add a named migration under migrations/ instead"
        );
    }

    #[test]
    fn request_close_migration_indexes_the_complete_batch_order() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        // Keep this identical to the auto-close batch query in thread_close.
        let sql = "SELECT updated_at, id FROM threads WHERE request = 1 AND status = 'waiting_operator' AND close_proposal IS NOT NULL AND updated_at < ?1 AND (updated_at, id) > (?2, ?3) AND NOT EXISTS (SELECT 1 FROM thread_cards WHERE thread = threads.id) ORDER BY updated_at, id LIMIT ?4";
        let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let plan = statement
            .query_map(
                rusqlite::params![
                    "2026-11-11T00:00:00.000Z",
                    "2020-01-01T00:00:00.000Z",
                    "cursor",
                    64
                ],
                |row| row.get::<_, String>(3),
            )
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("\n");
        assert!(plan.contains("threads_request_close"), "{plan}");
        assert!(!plan.contains("USE TEMP B-TREE"), "{plan}");
    }

    #[test]
    fn card_move_index_tolerates_malformed_events_and_serves_history() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        conn.execute("INSERT INTO events(id,project,kind,payload,at) VALUES ('bad','p','card','not JSON','now')", []).unwrap();
        conn.execute("UPDATE events SET payload = '{broken' WHERE id = 'bad'", [])
            .unwrap();
        let plan: String = conn
            .query_row(
                &format!("EXPLAIN QUERY PLAN {RECENT_PROJECT_MOVES_SQL}"),
                ["p"],
                |r| r.get(3),
            )
            .unwrap();
        assert!(plan.contains("events_card_moves"), "{plan}");
        assert!(
            crate::store::card_history::completed_visits(&conn, "p", "review", 20)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn last_moved_at_tolerates_malformed_events_and_uses_the_card_index() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        // A valid move, then a malformed event with a higher seq, so the
        // scan meets the malformed payload first.
        conn.execute(
            "INSERT INTO events(id,project,card,kind,payload,at) VALUES
             ('ok','p','T-1','card','{\"change\":\"moved\",\"to\":\"review\"}','2026-11-08T12:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events(id,project,card,kind,payload,at) VALUES
             ('bad','p','T-1','card','not JSON','2026-11-08T13:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute("UPDATE events SET payload = '{broken' WHERE id = 'bad'", [])
            .unwrap();
        let plan: String = conn
            .query_row(
                &format!("EXPLAIN QUERY PLAN {LAST_MOVED_AT_SQL}"),
                ["T-1"],
                |r| r.get(3),
            )
            .unwrap();
        assert!(plan.contains("SEARCH events USING INDEX"), "{plan}");
        assert!(!plan.contains("SCAN"), "{plan}");
        assert_eq!(
            crate::store::board::last_moved_at(&conn, "T-1")
                .unwrap()
                .as_deref(),
            Some("2026-11-08T12:00:00Z")
        );
        assert!(
            crate::store::board::last_moved_at(&conn, "T-9")
                .unwrap()
                .is_none()
        );
    }

    /// A card in the blocked column for [`crate::store::board::blocked_info`]:
    /// only `id`, `updated_at`, and `comments` reach the SQL.
    fn blocked_card(id: &str, updated_at: &str, comments: &[&str]) -> Card {
        serde_json::from_value(serde_json::json!({
            "id": id, "project": "p", "column": "blocked", "position": 1,
            "title": "t", "body": "b", "labels": [], "depends_on": [],
            "blocked_by": [],
            "comments": comments.iter().enumerate().map(|(i, body)| serde_json::json!({
                "id": format!("c-{i}"),
                "author": "operator", "author_name": "operator", "body": body,
                "created_at": "2026-11-08T00:00:00Z"
            })).collect::<Vec<_>>(),
            "links": [], "created_by": "operator", "created_by_name": "operator",
            "created_at": updated_at, "updated_at": updated_at, "archived": false
        }))
        .unwrap()
    }

    #[test]
    fn blocked_info_tolerates_malformed_events_and_uses_the_card_index() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        // A move to the blocked column with its comment, then a malformed
        // event with a higher seq, so the scan meets the malformed payload
        // first.
        conn.execute(
            "INSERT INTO events(id,project,card,kind,payload,at) VALUES
             ('ok','p','T-1','card','{\"change\":\"moved\",\"to_role\":\"blocked\",\"comment\":\"need a target\"}','2026-11-08T12:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events(id,project,card,kind,payload,at) VALUES
             ('bad','p','T-1','card','not JSON','2026-11-08T13:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute("UPDATE events SET payload = '{broken' WHERE id = 'bad'", [])
            .unwrap();
        let mut stmt = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {BLOCKED_INFO_SQL}"))
            .unwrap();
        let plan = stmt
            .query_map(["T-1", BLOCKED], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("\n");
        assert!(plan.contains("SEARCH events USING INDEX"), "{plan}");
        assert!(!plan.contains("SCAN"), "{plan}");
        assert_eq!(
            crate::store::board::blocked_info(
                &conn,
                &blocked_card("T-1", "2026-11-08T10:00:00Z", &["older"]),
            )
            .unwrap(),
            (
                "2026-11-08T12:00:00Z".to_owned(),
                Some("need a target".to_owned())
            )
        );
        // Without a blocked move (a card from before the event log), the
        // card's `updated_at` and its newest comment.
        assert_eq!(
            crate::store::board::blocked_info(
                &conn,
                &blocked_card("T-2", "2026-11-08T14:00:00Z", &["first", "still waiting"]),
            )
            .unwrap(),
            (
                "2026-11-08T14:00:00Z".to_owned(),
                Some("still waiting".to_owned())
            )
        );
    }

    #[test]
    fn agent_rpc_journal_upgrades_legacy_once_and_preserves_answers() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate_with(&mut conn, MIGRATIONS).unwrap();
        assert!(!conn.table_exists(None, "agent_rpc_answers").unwrap());
        migrate(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO agent_rpc_answers(agent, request_id, fingerprint, answer)
             VALUES ('agent', 'request', 'fingerprint', 'answer')",
            [],
        )
        .unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), 0);
        let answer: String = conn
            .query_row("SELECT answer FROM agent_rpc_answers", [], |row| row.get(0))
            .unwrap();
        assert_eq!(answer, "answer");
        let version: usize = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 97);
        let recorded: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE name = 'ter-260-agent-rpc-answers.sql')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(recorded);
    }

    #[test]
    fn column_options_backfill_todays_checks_on_a_populated_board() {
        const FILE: &str = "ter-1112-column-options.sql";
        let mut conn = Connection::open_in_memory().unwrap();
        migrate_with(&mut conn, MIGRATIONS).unwrap();
        let before: Vec<_> = NAMED_MIGRATIONS
            .iter()
            .copied()
            .filter(|(name, _)| *name != FILE)
            .collect();
        migrate_named(&mut conn, &before).unwrap();
        conn.execute_batch(
            "INSERT INTO projects(name,created_at,updated_at) VALUES ('p','t','t');
             INSERT INTO boards(project,created_at) VALUES ('p','t');
             INSERT INTO board_columns(project,name,position,role) VALUES
               ('p','backlog',0,NULL),('p','doing',1,'working'),('p','review',2,'review'),
               ('p','stuck',3,'blocked'),('p','done',4,NULL);",
        )
        .unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), 1);
        let mut q = conn
            .prepare(
                "SELECT name, require_comment_on_enter, require_dependencies_met,
                        terminal_for_members, clear_eta_on_enter, queue_release,
                        queue_batch_size, options_version
                 FROM board_columns ORDER BY position",
            )
            .unwrap();
        let rows: Vec<(String, i64, i64, i64, i64, String, Option<i64>, i64)> = q
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let want = |n: &str, c, d, t| (n.to_owned(), c, d, t, 1, "wip".to_owned(), None, 1);
        assert_eq!(
            rows,
            [
                want("backlog", 0, 0, 0),
                want("doing", 0, 1, 0),
                want("review", 0, 0, 0),
                want("stuck", 1, 0, 0),
                want("done", 0, 0, 1),
            ]
        );
        drop(q);
        // The CHECKs refuse a bad release and a zero batch.
        assert!(
            conn.execute("UPDATE board_columns SET queue_release = 'lifo'", [])
                .is_err()
        );
        assert!(
            conn.execute("UPDATE board_columns SET queue_batch_size = 0", [])
                .is_err()
        );
        assert_eq!(migrate(&mut conn).unwrap(), 0);
    }

    #[test]
    fn e2e_migration_preserves_release_history_and_all_job_kinds() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate_with(&mut conn, MIGRATIONS).unwrap();
        let before: Vec<_> = NAMED_MIGRATIONS
            .iter()
            .copied()
            .filter(|(name, _)| *name != "ter-719-job-e2e.sql")
            .collect();
        migrate_named(&mut conn, &before).unwrap();
        for kind in ["validate", "merge", "release"] {
            conn.execute("INSERT INTO jobs (id, project, card, kind, repo, branch, created_by, created_at) VALUES (?1, 'p', 'C1', ?1, 'app', 'main', 'operator', 'old')", [kind]).unwrap();
        }
        assert_eq!(migrate(&mut conn).unwrap(), 1);
        let old: String = conn
            .query_row("SELECT kind FROM jobs WHERE id = 'release'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(old, "release");
        for kind in ["release", "e2e"] {
            conn.execute("INSERT INTO jobs (id, project, card, kind, repo, branch, created_by, created_at) VALUES ('new-' || ?1, 'p', 'C1', ?1, 'app', 'main', 'operator', 'new')", [kind]).unwrap();
        }
        assert_eq!(migrate(&mut conn).unwrap(), 0);
    }

    #[test]
    fn agent_rpc_answer_age_counts_old_rows_from_the_upgrade() {
        const AGE: &str = "ter-555-agent-rpc-answers-age.sql";
        const COMPACTION: &str = "ter-555b-agent-rpc-compaction.sql";
        let mut conn = Connection::open_in_memory().unwrap();
        migrate_with(&mut conn, MIGRATIONS).unwrap();
        let before: Vec<_> = NAMED_MIGRATIONS
            .iter()
            .copied()
            .filter(|(name, _)| *name != AGE && *name != COMPACTION)
            .collect();
        migrate_named(&mut conn, &before).unwrap();
        conn.execute(
            "INSERT INTO agent_rpc_answers VALUES ('agent', 'request', 'fingerprint', 'answer')",
            [],
        )
        .unwrap();
        let start = chrono::Utc::now().timestamp();
        assert_eq!(migrate(&mut conn).unwrap(), 2);
        let created: i64 = conn
            .query_row("SELECT created_at FROM agent_rpc_answers", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(created >= start, "{created} < {start}");
        let plan: String = conn
            .query_row(
                "EXPLAIN QUERY PLAN SELECT rowid FROM agent_rpc_answers WHERE created_at < 1",
                [],
                |row| row.get(3),
            )
            .unwrap();
        assert!(plan.contains("agent_rpc_answers_created"), "{plan}");
    }

    #[test]
    fn embedded_sql_migration_is_loaded_and_applied() {
        let fixture = [(
            "ter-284-embedded.sql",
            include_str!("../tests/fixtures/named-migrations/ter-284-embedded.sql"),
        )];

        let mut conn = Connection::open_in_memory().unwrap();
        assert_eq!(migrate_named(&mut conn, &fixture).unwrap(), 1);
        assert!(
            conn.table_exists(None, "embedded_migration_fixture")
                .unwrap()
        );
        assert_eq!(migrate_named(&mut conn, &fixture).unwrap(), 0);
        let name: String = conn
            .query_row("SELECT name FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "ter-284-embedded.sql");
    }

    /// The named migrations are compiled into the binary, in filename order,
    /// and match `migrations/` (TER-492: a debug `rust-embed` read them from
    /// the source tree at run time, so a copied test binary had none).
    #[test]
    fn named_migrations_are_compiled_in_and_complete() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        let mut on_disk: Vec<(String, String)> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "sql"))
            .map(|p| {
                let name = p.file_name().unwrap().to_str().unwrap().to_owned();
                (name, std::fs::read_to_string(&p).unwrap())
            })
            .collect();
        on_disk.sort();
        let compiled: Vec<(String, String)> = NAMED_MIGRATIONS
            .iter()
            .map(|(n, sql)| ((*n).to_owned(), (*sql).to_owned()))
            .collect();
        assert!(
            compiled
                .iter()
                .any(|(n, _)| n == "ter-249-resource-reservations.sql")
        );
        assert_eq!(compiled, on_disk);
    }

    #[test]
    fn named_migrations_upgrade_legacy_and_allow_earlier_names() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate_with(&mut conn, MIGRATIONS).unwrap();
        conn.execute(
            "INSERT INTO projects (name, created_at, updated_at) VALUES ('preserved', ?1, ?1)",
            [now()],
        )
        .unwrap();
        let later = ("ter-300-later.sql", "CREATE TABLE later (id INTEGER);");
        let earlier = ("ter-284-earlier.sql", "CREATE TABLE earlier (id INTEGER);");
        assert_eq!(migrate_named(&mut conn, &[later]).unwrap(), 1);
        assert_eq!(migrate_named(&mut conn, &[earlier, later]).unwrap(), 1);
        assert_eq!(migrate_named(&mut conn, &[earlier, later]).unwrap(), 0);
        assert_eq!(
            migrate(&mut conn).unwrap(),
            migration_count() - MIGRATIONS.len()
        );
        assert_eq!(migrate(&mut conn).unwrap(), 0);
        let version: usize = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len());
        let project: String = conn
            .query_row("SELECT name FROM projects", [], |r| r.get(0))
            .unwrap();
        assert_eq!(project, "preserved");
    }

    fn seed_migration_dependencies(conn: &Connection) {
        conn.execute_batch(
            "INSERT INTO projects (name, created_at, updated_at) VALUES ('p', 't', 't');
             INSERT INTO boards (project, created_at) VALUES ('p', 't');
             INSERT INTO board_columns (id, project, name, position) VALUES (1, 'p', 'ready', 0);
             INSERT INTO cards (id, project, number, column_id, position, title,
                                created_by, created_at, updated_at)
             SELECT 'P-' || n, 'p', n, 1, n, 'migration fixture', 'operator', 't', 't'
             FROM (SELECT 1 AS n UNION ALL SELECT 2 UNION ALL SELECT 3
                   UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6);
             INSERT INTO card_deps (card, depends_on) VALUES
               ('P-1', 'P-4'), ('P-1', 'P-5'), ('P-1', 'P-6'),
               ('P-2', 'P-4'), ('P-3', 'P-5');",
        )
        .unwrap();
    }

    fn assert_migrated_dependencies(conn: &Connection) {
        let mut query = conn
            .prepare("SELECT card, generation, mode, action, setter FROM card_waits ORDER BY card")
            .unwrap();
        let rows: Vec<(String, String, String, String, String)> = query
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows.len(), 3);
        for (i, (card, generation, mode, action, setter)) in rows.iter().enumerate() {
            assert_eq!(card, &format!("P-{}", i + 1));
            assert_eq!(generation.len(), 32);
            assert!(
                generation
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            );
            assert_eq!(
                (mode.as_str(), action.as_str(), setter.as_str()),
                ("all", "{}", "operator")
            );
        }
        assert_eq!(
            conn.query_row("SELECT count(*) FROM card_deps", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            5
        );
    }

    #[test]
    fn seeded_waits_complete_silently_once_across_pages() {
        let mut conn = Connection::open_in_memory().unwrap();
        let before: Vec<_> = NAMED_MIGRATIONS
            .iter()
            .copied()
            .filter(|(name, _)| *name != "ter-970-seeded-waits.sql")
            .collect();
        migrate_with(&mut conn, MIGRATIONS).unwrap();
        migrate_named(&mut conn, &before).unwrap();
        seed_migration_dependencies(&conn);
        conn.execute_batch(
            "INSERT INTO board_columns(id,project,name,role,position) VALUES (2,'p','finished','done',1);
             UPDATE cards SET column_id=2 WHERE id IN ('P-4','P-6');
             INSERT INTO card_waits(card,generation,mode,action,setter)
             SELECT DISTINCT card,'legacy','all','{}','operator' FROM card_deps;
             WITH RECURSIVE n(x) AS (VALUES(10) UNION ALL SELECT x+1 FROM n WHERE x<270)
             INSERT INTO cards(id,project,number,column_id,position,title,created_by,created_at,updated_at)
             SELECT 'P-'||x,'p',x,1,x,'paged seed','operator','t','t' FROM n;
             INSERT INTO card_deps(card,depends_on) SELECT id,'P-4' FROM cards WHERE number>=10;
             INSERT INTO card_waits(card,generation,mode,action,setter)
             SELECT id,'legacy','all','{}','operator' FROM cards WHERE number>=10;"
        ).unwrap();
        let counts = |conn: &Connection| -> (i64, i64) {
            (
                conn.query_row("SELECT count(*) FROM events", [], |r| r.get(0))
                    .unwrap(),
                conn.query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
                    .unwrap(),
            )
        };
        let initial = counts(&conn);
        assert_eq!(migrate(&mut conn).unwrap(), 1);
        let met: i64 = conn
            .query_row(
                "SELECT count(*) FROM card_waits WHERE met_at IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(met, 262);
        let timestamp: String = conn
            .query_row("SELECT met_at FROM card_waits WHERE card='P-2'", [], |r| {
                r.get(0)
            })
            .unwrap();
        for card in ["P-1", "P-3"] {
            assert!(
                conn.query_row("SELECT met_at FROM card_waits WHERE card=?1", [card], |r| r
                    .get::<_, Option<String>>(0))
                    .unwrap()
                    .is_none()
            );
        }
        assert_eq!(counts(&conn), initial);
        assert_eq!(migrate(&mut conn).unwrap(), 0);
        assert_eq!(
            conn.query_row("SELECT met_at FROM card_waits WHERE card='P-2'", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
            timestamp
        );
        assert_eq!(counts(&conn), initial);
    }

    #[test]
    fn card_waits_migration_upgrades_populated_055_database() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        migrate_with(&mut conn, MIGRATIONS).unwrap();
        // These are the only named migrations added after 0.55.0.
        let before: Vec<_> = NAMED_MIGRATIONS
            .iter()
            .copied()
            .filter(|(name, _)| {
                !matches!(
                    *name,
                    "ter-712-card-waits.sql"
                        | "ter-893-legacy-listing-expiry.sql"
                        | "ter-970-seeded-waits.sql"
                )
            })
            .collect();
        migrate_named(&mut conn, &before).unwrap();
        seed_migration_dependencies(&conn);
        assert_eq!(migrate(&mut conn).unwrap(), 3);
        assert_migrated_dependencies(&conn);
        assert_eq!(migrate(&mut conn).unwrap(), 0);
        assert_migrated_dependencies(&conn);
    }

    #[test]
    fn named_migrations_upgrade_populated_legacy_database() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        migrate_with(&mut conn, MIGRATIONS).unwrap();
        seed_migration_dependencies(&conn);
        assert_eq!(
            migrate_named(&mut conn, NAMED_MIGRATIONS).unwrap(),
            NAMED_MIGRATIONS.len()
        );
        assert_migrated_dependencies(&conn);
        assert_eq!(migrate(&mut conn).unwrap(), 0);
    }

    #[test]
    fn named_migrations_roll_back_failed_schema_and_record() {
        let mut conn = Connection::open_in_memory().unwrap();
        let good = ("good.sql", "CREATE TABLE good (id INTEGER);");
        let err = migrate_named(
            &mut conn,
            &[
                good,
                ("bad.sql", "CREATE TABLE bad (id INTEGER); invalid SQL"),
            ],
        )
        .unwrap_err();
        assert!(err.to_string().contains("migration bad.sql failed"));
        assert!(conn.table_exists(None, "good").unwrap());
        assert!(!conn.table_exists(None, "bad").unwrap());
        assert_eq!(
            migrate_named(
                &mut conn,
                &[good, ("bad.sql", "CREATE TABLE bad (id INTEGER);")]
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn fresh_migrations_roll_back_together() {
        let mut conn = Connection::open_in_memory().unwrap();
        let err = migrate_with(
            &mut conn,
            &["CREATE TABLE first (id INTEGER);", "invalid SQL"],
        )
        .unwrap_err();
        assert!(err.to_string().contains("migration 2 failed"));
        let version: usize = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 0);
        assert!(!conn.table_exists(None, "first").unwrap());
    }

    #[test]
    fn upgrades_keep_each_successful_migration() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        assert!(
            migrate_with(
                &mut conn,
                &["unused", "CREATE TABLE second (id INTEGER);", "invalid SQL"],
            )
            .is_err()
        );
        let version: usize = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 2);
        assert!(conn.table_exists(None, "second").unwrap());
    }

    #[test]
    fn migrations_create_tables_and_are_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), migration_count());
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
            "subscriptions",
            "quota_samples",
            "usage_samples",
            "login_sessions",
            "quota_holds",
            "quota_alerts",
            "quota_notices",
            "hosts",
            "targets",
            "fleets",
            "fleet_objects",
            "target_leases",
            "schedules",
            "schedule_runs",
            "waits",
            "watches",
            "account_defaults",
            "questions",
            "thread_resumes",
            "secrets",
            "secret_grants",
            "harness_updates",
            "test_runs",
            "test_suites",
            "test_cases",
            "test_run_cards",
            "test_run_labels",
            "artifact_labels",
            "spawns",
            "monitors",
            "webhook_deliveries",
            "calendar_events",
            "calendar_assignees",
            "calendar_exceptions",
            "calendar_watches",
            "calendar_notices",
            "calendar_state",
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

    /// TER-58: migration 80 adds the label tables and keeps the rows of
    /// the version tables.
    #[test]
    fn migration_80_keeps_the_version_rows() {
        let mut conn = Connection::open_in_memory().unwrap();
        for sql in &MIGRATIONS[..79] {
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", 79).unwrap();
        conn.execute_batch(
            "INSERT INTO projects (name, created_at, updated_at) VALUES ('p', 't', 't');
             INSERT INTO versions (id, project, name, status, created_by, created_at, updated_at)
               VALUES ('V1', 'p', 'v1.0.0', 'released', 'operator', 't', 't');
             INSERT INTO version_reviews (id, version, author, decision, created_at)
               VALUES ('R1', 'V1', 'operator', 'approved', 't');",
        )
        .unwrap();
        assert_eq!(migrate_with(&mut conn, &MIGRATIONS[..80]).unwrap(), 1);
        let n: i64 = conn
            .query_row(
                "SELECT (SELECT count(*) FROM versions WHERE status = 'released')
                      + (SELECT count(*) FROM version_reviews)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2);
        assert_eq!(migrate(&mut conn).unwrap(), migration_count() - 80);
        assert_eq!(migrate(&mut conn).unwrap(), 0);
        for table in [
            "versions",
            "version_cards",
            "version_artifacts",
            "version_reviews",
            "test_run_versions",
        ] {
            assert!(!conn.table_exists(None, table).unwrap(), "{table}");
            let indexes: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND tbl_name = ?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(indexes, 0, "{table}");
        }
        for table in ["test_run_labels", "artifact_labels"] {
            let n: i64 = conn
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "{table}");
        }
    }

    /// TER-142: the rebuild of `secret_grants` keeps each grant, and the
    /// new table takes the level `run` with its `targets`.
    #[test]
    fn migration_of_the_run_level_keeps_the_grants() {
        let mut conn = Connection::open_in_memory().unwrap();
        let at = MIGRATIONS
            .iter()
            .position(|m| m.contains("secret_grants_v81"))
            .unwrap();
        for m in &MIGRATIONS[..at] {
            conn.execute_batch(m).unwrap();
        }
        conn.pragma_update(None, "user_version", at).unwrap();
        conn.execute_batch(
            "INSERT INTO secrets (id, scope, name, kind, created_at, updated_at, created_by)
               VALUES ('S1', 'global', 'k', 'api_key', 't', 't', 'operator');
             INSERT INTO secret_grants (secret_id, grantee, level, env, created_by, created_at)
               VALUES ('S1', 'project:p', 'read', 'K', 'operator', 't'),
                      ('S1', 'agent:A1', 'use', NULL, 'operator', 't');",
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO secret_grants (secret_id, grantee, level, created_by, created_at)
                 VALUES ('S1', 'agent:A2', 'run', 'operator', 't')",
                [],
            )
            .is_err()
        );
        assert_eq!(migrate(&mut conn).unwrap(), migration_count() - at);
        let rows: Vec<(String, String, Option<String>)> = conn
            .prepare("SELECT grantee, level, env FROM secret_grants ORDER BY grantee")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            [
                ("agent:A1".to_owned(), "use".to_owned(), None),
                (
                    "project:p".to_owned(),
                    "read".to_owned(),
                    Some("K".to_owned())
                ),
            ]
        );
        conn.execute(
            "INSERT INTO secret_grants (secret_id, grantee, level, created_by, created_at, targets)
             VALUES ('S1', 'agent:A2', 'run', 'operator', 't', '[\"box\"]')",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO secret_grants (secret_id, grantee, level, created_by, created_at)
                 VALUES ('S1', 'agent:A3', 'write', 'operator', 't')",
                [],
            )
            .is_err()
        );
        // The cascade from the secret still holds.
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        conn.execute("DELETE FROM secrets WHERE id = 'S1'", [])
            .unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM secret_grants", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    /// Migration 83 keeps the threads and their messages in order, and
    /// takes the status `tracking`. Found by its content.
    #[test]
    fn migration_83_keeps_threads_and_takes_tracking() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        let n = MIGRATIONS
            .iter()
            .position(|m| m.contains("threads_tracking"))
            .unwrap();
        for (i, sql) in MIGRATIONS.iter().enumerate().take(n) {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", i + 1).unwrap();
        }
        conn.execute_batch(
            "INSERT INTO threads (id, topic, scope, opened_by, status, created_at, updated_at,
                                  reminded_at, close_proposal)
               VALUES ('T1', 'Cache', 'global', 'operator', 'waiting_operator', 't', 'u',
                       'r', '{}');
             INSERT INTO thread_messages (id, thread, author, kind, text, at)
               VALUES ('M2', 'T1', 'operator', 'message', 'first', 'a');
             INSERT INTO thread_messages (id, thread, author, kind, text, at)
               VALUES ('M1', 'T1', 'A1', 'message', 'second', 'a');",
        )
        .unwrap();
        assert!(
            conn.execute("UPDATE threads SET status = 'tracking'", [])
                .is_err()
        );
        migrate(&mut conn).unwrap();
        let row: (String, String, String, Option<String>) = conn
            .query_row(
                "SELECT status, reminded_at, close_proposal, tracking_at FROM threads",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            ("waiting_operator".into(), "r".into(), "{}".into(), None)
        );
        let texts: Vec<String> = conn
            .prepare("SELECT text FROM thread_messages WHERE thread = 'T1' ORDER BY rowid")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(texts, ["first", "second"]);
        conn.execute(
            "UPDATE threads SET status = 'tracking', tracking_at = 'v'",
            [],
        )
        .unwrap();
        assert!(
            conn.execute("UPDATE threads SET status = 'bogus'", [])
                .is_err()
        );
        // The cascade from threads still works.
        conn.execute("DELETE FROM threads WHERE id = 'T1'", [])
            .unwrap();
        let left: i64 = conn
            .query_row("SELECT count(*) FROM thread_messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0);
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
        assert_eq!(migrate(&mut conn).unwrap(), migration_count() - 1);
        let (seq, kind): (i64, String) = conn
            .query_row("SELECT seq, kind FROM events WHERE id = 'E1'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((seq, kind.as_str()), (1, "turn_end"));
    }

    #[test]
    fn migration_6_keeps_mail_and_allows_system() {
        let mut conn = Connection::open_in_memory().unwrap();
        for sql in &MIGRATIONS[..5] {
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", 5).unwrap();
        conn.execute_batch(
            "INSERT INTO messages (id, thread_id, from_agent, from_kind, to_agent, to_kind, body,
                                   state, created_at, in_reply_to)
               VALUES ('M1', 'T1', 'operator', 'operator', 'A1', 'agent', 'hi', 'read', 't', 'M0');",
        )
        .unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), migration_count() - 5);
        let (body, state, reply): (String, String, String) = conn
            .query_row(
                "SELECT body, state, in_reply_to FROM messages WHERE id = 'M1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (body.as_str(), state.as_str(), reply.as_str()),
            ("hi", "read", "M0")
        );
        conn.execute(
            "INSERT INTO messages (id, thread_id, from_agent, from_kind, to_agent, to_kind, body,
                                   created_at)
               VALUES ('M2', 'T2', 'system', 'system', 'operator', 'operator', 'quota', 't')",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO messages (id, thread_id, from_agent, to_agent, to_kind, body, created_at)
                   VALUES ('M3', 'T3', 'operator', 'system', 'system', 'x', 't')",
                [],
            )
            .is_err()
        );
    }

    #[test]
    fn migration_7_starts_runner_seq_at_zero() {
        let mut conn = Connection::open_in_memory().unwrap();
        for sql in &MIGRATIONS[..6] {
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", 6).unwrap();
        conn.execute_batch(
            "INSERT INTO agents (id, name, project, harness, workspace, created_at, updated_at)
               VALUES ('A1', 'a', 'p', 'claude', '/w', 't', 't');",
        )
        .unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), migration_count() - 6);
        let seq: i64 = conn
            .query_row("SELECT runner_seq FROM agents WHERE id = 'A1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(seq, 0);
    }

    #[test]
    fn migration_8_adds_a_null_card_id() {
        let mut conn = Connection::open_in_memory().unwrap();
        for sql in &MIGRATIONS[..7] {
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", 7).unwrap();
        conn.execute_batch(
            "INSERT INTO usage_samples (agent, project, message_id, at)
               VALUES ('A1', 'p', 'm1', 't');",
        )
        .unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), migration_count() - 7);
        let card: Option<String> = conn
            .query_row("SELECT card_id FROM usage_samples", [], |r| r.get(0))
            .unwrap();
        assert_eq!(card, None);
    }

    #[test]
    fn migration_9_labels_fleet_objects_and_keeps_child_rows() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        for sql in &MIGRATIONS[..8] {
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", 8).unwrap();
        // The fleet tables as `fleet.rs` made them before migration 9.
        conn.execute_batch(
            "CREATE TABLE fleets (name TEXT PRIMARY KEY, description TEXT, origin TEXT NOT NULL,
                                  content TEXT NOT NULL, applied_at TEXT NOT NULL,
                                  applied_by TEXT NOT NULL);
             CREATE TABLE fleet_objects (kind TEXT NOT NULL, key TEXT NOT NULL,
                                         fleet TEXT NOT NULL, project TEXT,
                                         applied TEXT NOT NULL, applied_at TEXT NOT NULL,
                                         PRIMARY KEY (kind, key));
             INSERT INTO fleets VALUES ('f', NULL, 'api', '', 't', 'operator');
             INSERT INTO fleet_objects VALUES
               ('project', 'p', 'f', 'p', '{}', 't'),
               ('project', 'byfile', 'f', 'byfile', '{}', 't'),
               ('agent', 'p/a', 'f', 'p', '{}', 't'),
               ('target', 'box', 'f', 'p', '{}', 't'),
               ('subscription', 's', 'f', NULL, '{}', 't');
             INSERT INTO projects (name, created_at, updated_at, source)
               VALUES ('p', 't', 't', 'api'), ('byfile', 't', 't', 'file'),
                      ('own', 't', 't', 'api');
             INSERT INTO repos (id, project, name, url, created_at)
               VALUES ('R1', 'p', 'r', 'u', 't');
             INSERT INTO boards (project, created_at) VALUES ('p', 't');
             INSERT INTO agents (id, name, project, harness, workspace, created_at, updated_at)
               VALUES ('A1', 'a', 'p', 'claude', '/w', 't', 't'),
                      ('A2', 'b', 'p', 'claude', '/w', 't', 't');
             INSERT INTO sessions (id, agent_id, started_at) VALUES ('S1', 'A1', 't');
             INSERT INTO roles (agent_id, role, project, granted_by, granted_at)
               VALUES ('A1', 'project_admin', 'p', 'operator', 't');
             INSERT INTO targets (name, project, kind, def, created_at, updated_at)
               VALUES ('box', 'p', 'ssh', '{}', 't', 't');
             INSERT INTO subscriptions (name, provider, created_at, updated_at)
               VALUES ('s', 'anthropic', 't', 't');
             INSERT INTO quota_samples (subscription, window, used_pct, at)
               VALUES ('s', '5h', 1.0, 't');",
        )
        .unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), migration_count() - 8);
        let source = |sql: &str| -> String { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(
            source("SELECT source FROM projects WHERE name = 'p'"),
            "fleet:f"
        );
        assert_eq!(
            source("SELECT source FROM projects WHERE name = 'byfile'"),
            "file"
        );
        assert_eq!(
            source("SELECT source FROM projects WHERE name = 'own'"),
            "api"
        );
        assert_eq!(
            source("SELECT source FROM agents WHERE id = 'A1'"),
            "fleet:f"
        );
        assert_eq!(source("SELECT source FROM agents WHERE id = 'A2'"), "api");
        assert_eq!(source("SELECT source FROM targets"), "fleet:f");
        assert_eq!(source("SELECT source FROM subscriptions"), "fleet:f");
        // Old repos and targets count as set by an admin (PLAN §16b.6a).
        for table in ["repos", "targets"] {
            let admin: bool = conn
                .query_row(&format!("SELECT credential_admin FROM {table}"), [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert!(admin, "{table}");
        }
        // No cascade: the child rows stay.
        for table in ["repos", "boards", "sessions", "roles", "quota_samples"] {
            let n: i64 = conn
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 1, "{table}");
        }
        let fleets: i64 = conn
            .query_row("SELECT count(*) FROM fleets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fleets, 1);
        // The new CHECK: `file`, `api`, and `fleet:<name>`.
        conn.execute(
            "INSERT INTO projects (name, created_at, updated_at, source) VALUES ('x', 't', 't', 'fleet:g')",
            [],
        )
        .unwrap();
        for bad in ["bogus", "fleet:"] {
            assert!(
                conn.execute(
                    "INSERT INTO targets (name, kind, def, source, created_at, updated_at)
                       VALUES ('t2', 'ssh', '{}', ?1, 't', 't')",
                    [bad],
                )
                .is_err(),
                "{bad}"
            );
        }
        let fk: i64 = conn
            .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(fk, 0);
    }

    /// Migration 24 makes the status column again with `waiting`, and
    /// keeps each stored status and the rows that point at the agent.
    #[test]
    fn migration_24_keeps_the_status_and_takes_waiting() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        for (i, sql) in MIGRATIONS.iter().enumerate().take(23) {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", i + 1).unwrap();
        }
        conn.execute_batch(
            "INSERT INTO agents (id, name, project, harness, workspace, status, created_at, updated_at)
               VALUES ('A1', 'a', 'p', 'claude', '/w', 'blocked', 't', 't');
             INSERT INTO sessions (id, agent_id, current, started_at) VALUES ('S1', 'A1', 1, 't');",
        )
        .unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), migration_count() - 23);
        let status: String = conn
            .query_row("SELECT status FROM agents WHERE id = 'A1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(status, "blocked");
        conn.execute("UPDATE agents SET status = 'waiting' WHERE id = 'A1'", [])
            .unwrap();
        assert!(
            conn.execute("UPDATE agents SET status = 'bogus' WHERE id = 'A1'", [])
                .is_err()
        );
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM sessions WHERE agent_id = 'A1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    /// Migration 34 merges the rows that resumes of one harness session
    /// made: the earliest row stays, with the earliest start, the end of
    /// the newest row, `resumes` = the other rows, and their events (so
    /// their turns). A session with one row, and a row with no harness
    /// session, stay as they are.
    #[test]
    fn migration_34_merges_the_rows_of_one_harness_session() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        for (i, sql) in MIGRATIONS.iter().enumerate().take(33) {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", i + 1).unwrap();
        }
        conn.execute_batch(
            "INSERT INTO agents (id, name, project, harness, workspace, created_at, updated_at)
               VALUES ('A1', 'a', 'p', 'claude', '/w', 't', 't'),
                      ('A2', 'b', 'p', 'claude', '/w', 't', 't');
             -- A1: H1 resumed twice (the last row is current), then H2 once.
             INSERT INTO sessions (id, agent_id, harness_session_id, current, started_at,
                                   ended_at, end_reason) VALUES
               ('S1', 'A1', 'H1', 0, 't1', 'e1', 'restarted'),
               ('S2', 'A1', 'H1', 0, 't2', 'e2', 'daemon_restart'),
               ('S3', 'A1', NULL, 0, 't3', 'e3', 'start_failed'),
               ('S4', 'A1', 'H2', 0, 't4', 'e4', 'stopped'),
               ('S5', 'A1', 'H1', 1, 't5', NULL, NULL),
               -- A2: H1 too (another agent), two ended rows.
               ('T1', 'A2', 'H1', 0, 'u1', 'f1', 'restarted'),
               ('T2', 'A2', 'H1', 0, 'u2', 'f2', 'stopped');
             INSERT INTO events (id, agent_id, session_id, kind, payload, at) VALUES
               ('E1', 'A1', 'S1', 'turn_start', '{}', 't1'),
               ('E2', 'A1', 'S2', 'turn_start', '{}', 't2'),
               ('E3', 'A1', 'S5', 'turn_start', '{}', 't5'),
               ('E4', 'A1', 'S5', 'turn_start', '{}', 't5'),
               ('E5', 'A1', 'S4', 'turn_start', '{}', 't4'),
               ('E6', 'A2', 'T2', 'turn_start', '{}', 'u2');",
        )
        .unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), migration_count() - 33);
        type Row = (
            String,
            Option<String>,
            i64,
            String,
            Option<String>,
            Option<String>,
            i64,
            Option<String>,
            i64,
        );
        let rows: Vec<Row> = conn
            .prepare(
                "SELECT s.id, s.harness_session_id, s.current, s.started_at, s.ended_at,
                        s.end_reason, s.resumes, s.resumed_at,
                        (SELECT count(*) FROM events e
                         WHERE e.session_id = s.id AND e.kind = 'turn_start')
                 FROM sessions s ORDER BY s.id",
            )
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                ))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let o = |s: &str| Some(s.to_owned());
        assert_eq!(
            rows,
            [
                (
                    "S1".into(),
                    o("H1"),
                    1,
                    "t1".into(),
                    None,
                    None,
                    2,
                    o("t5"),
                    4
                ),
                (
                    "S3".into(),
                    None,
                    0,
                    "t3".into(),
                    o("e3"),
                    o("start_failed"),
                    0,
                    None,
                    0
                ),
                (
                    "S4".into(),
                    o("H2"),
                    0,
                    "t4".into(),
                    o("e4"),
                    o("stopped"),
                    0,
                    None,
                    1
                ),
                (
                    "T1".into(),
                    o("H1"),
                    0,
                    "u1".into(),
                    o("f2"),
                    o("stopped"),
                    1,
                    o("u2"),
                    1
                ),
            ]
        );
        let fk: i64 = conn
            .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(fk, 0);
    }

    /// Migration 29 makes the wait kind column again with `question`, and
    /// keeps each stored wait.
    #[test]
    fn migration_29_keeps_waits_and_takes_question() {
        let mut conn = Connection::open_in_memory().unwrap();
        for (i, sql) in MIGRATIONS.iter().enumerate().take(28) {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", i + 1).unwrap();
        }
        conn.execute(
            "INSERT INTO waits (id, agent, project, kind, spec, summary, created_by, created_at)
               VALUES ('W1', 'A1', 'p', 'card', '{}', 'card X in done', 'operator', 't')",
            [],
        )
        .unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), migration_count() - 28);
        let kind: String = conn
            .query_row("SELECT kind FROM waits WHERE id = 'W1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kind, "card");
        conn.execute(
            "INSERT INTO waits (id, agent, project, kind, spec, summary, created_by, created_at)
               VALUES ('W2', 'A1', 'p', 'question', '{}', 'question x', 'operator', 't')",
            [],
        )
        .unwrap();
        assert!(
            conn.execute("UPDATE waits SET kind = 'bogus' WHERE id = 'W1'", [])
                .is_err()
        );
    }

    #[test]
    fn run_cleanup_migration_never_backfills_legacy_success() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate_with(&mut conn, MIGRATIONS).unwrap();
        let before: Vec<_> = NAMED_MIGRATIONS
            .iter()
            .copied()
            .filter(|(name, _)| *name != "ter-1059-run-cleanup.sql")
            .collect();
        migrate_named(&mut conn, &before).unwrap();
        conn.execute("INSERT INTO waits (id,agent,project,kind,spec,summary,state,result,created_by,created_at) VALUES ('legacy','owner','p','process','{}','old run','cancelled','{\"outcome\":\"cancelled\"}','operator','old')", []).unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), 1);
        let count: i64 = conn
            .query_row("SELECT count(*) FROM run_cleanup", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(migrate(&mut conn).unwrap(), 0);
        let result: String = conn
            .query_row("SELECT result FROM waits WHERE id='legacy'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(result, "{\"outcome\":\"cancelled\"}");
        conn.execute("INSERT INTO run_cleanup (operation_id,wait_id,project,owner,requester,schema_version,requested_at,updated_at,phase,status,reason) VALUES ('op','legacy','p','owner','operator',1,'t','t','queued','pending','intent_recorded')", []).unwrap();
        assert!(
            conn.execute("UPDATE run_cleanup SET status='verified_terminated'", [])
                .is_err()
        );
        assert!(
            conn.execute("UPDATE run_cleanup SET phase='replayed'", [])
                .is_err()
        );
        assert!(conn.execute("INSERT INTO run_cleanup SELECT 'duplicate',wait_id,project,owner,requester,session,target,schema_version,requested_at,updated_at,phase,status,reason,diagnostic FROM run_cleanup", []).is_err());
    }

    #[test]
    fn migration_30_keeps_questions_and_releases_old_held_mail() {
        let mut conn = Connection::open_in_memory().unwrap();
        for (i, sql) in MIGRATIONS.iter().enumerate().take(29) {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", i + 1).unwrap();
        }
        conn.execute(
            "INSERT INTO questions (id, agent, project, text, status, answer, thread_id,
                                    created_at, updated_at)
               VALUES ('Q1', 'A1', 'p', 'Ship?', 'answered', 'yes', 'T0', 't', 't')",
            [],
        )
        .unwrap();
        // Thread T1: a held message, then an operator reset. Thread T2: a
        // held message with no reset.
        for (id, thread, from_kind, held) in [
            ("M1", "T1", "agent", 1),
            ("M2", "T1", "operator", 0),
            ("M3", "T2", "agent", 1),
        ] {
            conn.execute(
                "INSERT INTO messages (id, thread_id, from_agent, from_kind, to_agent, to_kind,
                                       body, state, created_at, held)
                   VALUES (?1, ?2, 'a', ?3, 'b', 'agent', 'x', 'delivered', 't', ?4)",
                rusqlite::params![id, thread, from_kind, held],
            )
            .unwrap();
        }
        assert_eq!(migrate(&mut conn).unwrap(), migration_count() - 29);
        let (kind, status): (String, String) = conn
            .query_row(
                "SELECT kind, status FROM questions WHERE id = 'Q1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((kind.as_str(), status.as_str()), ("question", "answered"));
        conn.execute(
            "UPDATE questions SET kind = 'notice', status = 'acknowledged' WHERE id = 'Q1'",
            [],
        )
        .unwrap();
        assert!(
            conn.execute("UPDATE questions SET status = 'bogus' WHERE id = 'Q1'", [])
                .is_err()
        );
        assert!(
            conn.execute("UPDATE questions SET kind = 'bogus' WHERE id = 'Q1'", [])
                .is_err()
        );
        let released = |id: &str| -> Option<String> {
            conn.query_row(
                "SELECT released_by FROM messages WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(released("M1").as_deref(), Some("system"));
        assert_eq!(released("M3"), None);
    }

    #[test]
    fn migration_32_keeps_the_priority_and_takes_critical() {
        let mut conn = Connection::open_in_memory().unwrap();
        for (i, sql) in MIGRATIONS.iter().enumerate().take(31) {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", i + 1).unwrap();
        }
        assert!(
            conn.execute(
                "INSERT INTO questions (id, agent, project, text, priority, thread_id,
                                        created_at, updated_at)
                   VALUES ('Q0', 'A1', 'p', 'x', 'critical', 'T0', 't', 't')",
                [],
            )
            .is_err(),
            "before 32, the CHECK refuses critical"
        );
        conn.execute(
            "INSERT INTO questions (id, agent, project, text, priority, thread_id,
                                    created_at, updated_at)
               VALUES ('Q1', 'A1', 'p', 'Ship?', 'urgent', 'T0', 't', 't')",
            [],
        )
        .unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), migration_count() - 31);
        let priority = |id: &str| -> String {
            conn.query_row("SELECT priority FROM questions WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert_eq!(priority("Q1"), "urgent");
        conn.execute(
            "UPDATE questions SET priority = 'critical' WHERE id = 'Q1'",
            [],
        )
        .unwrap();
        assert_eq!(priority("Q1"), "critical");
        assert!(
            conn.execute(
                "UPDATE questions SET priority = 'bogus' WHERE id = 'Q1'",
                []
            )
            .is_err()
        );
        conn.execute(
            "INSERT INTO questions (id, agent, project, text, thread_id, created_at, updated_at)
               VALUES ('Q2', 'A1', 'p', 'x', 'T0', 't', 't')",
            [],
        )
        .unwrap();
        assert_eq!(priority("Q2"), "normal");
    }

    #[test]
    fn migration_25_keeps_targets_and_allows_display_kinds() {
        let mut conn = Connection::open_in_memory().unwrap();
        for sql in &MIGRATIONS[..23] {
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", 23).unwrap();
        conn.execute(
            "INSERT INTO targets (name, project, kind, def, source, created_at, updated_at)
               VALUES ('box', 'p', 'docker', '{}', 'fleet:f', 't', 't')",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO targets (name, kind, def, created_at, updated_at)
                   VALUES ('v0', 'vnc', '{}', 't', 't')",
                [],
            )
            .is_err(),
            "the old CHECK refuses vnc"
        );
        assert_eq!(migrate(&mut conn).unwrap(), migration_count() - 23);
        let (kind, source): (String, String) = conn
            .query_row(
                "SELECT kind, source FROM targets WHERE name = 'box'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((kind.as_str(), source.as_str()), ("docker", "fleet:f"));
        for k in ["vnc", "rdp", "desktop"] {
            conn.execute(
                "INSERT INTO targets (name, kind, def, created_at, updated_at)
                   VALUES (?1, ?1, '{}', 't', 't')",
                [k],
            )
            .unwrap();
        }
        assert!(
            conn.execute(
                "INSERT INTO targets (name, kind, def, created_at, updated_at)
                   VALUES ('x', 'telnet', '{}', 't', 't')",
                [],
            )
            .is_err()
        );
    }

    #[test]
    fn migration_31_keeps_deliveries_and_allows_agent() {
        let mut conn = Connection::open_in_memory().unwrap();
        for sql in &MIGRATIONS[..30] {
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", 30).unwrap();
        // No agent row: the foreign keys are off on this connection.
        conn.pragma_update(None, "foreign_keys", false).unwrap();
        conn.execute(
            "INSERT INTO deliveries (agent_id, origin, hash, at) VALUES ('A1', 'system', 'h1', 't')",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO deliveries (agent_id, origin, hash, at) VALUES ('A1', 'agent', 'h2', 't')",
                [],
            )
            .is_err(),
            "the old CHECK refuses agent"
        );
        assert_eq!(migrate(&mut conn).unwrap(), migration_count() - 30);
        let origin: String = conn
            .query_row("SELECT origin FROM deliveries WHERE hash = 'h1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(origin, "system");
        conn.execute(
            "INSERT INTO deliveries (agent_id, origin, hash, at) VALUES ('A1', 'agent', 'h2', 't')",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO deliveries (agent_id, origin, hash, at) VALUES ('A1', 'robot', 'h3', 't')",
                [],
            )
            .is_err()
        );
    }

    /// Migration 37 keeps every session row and drops the cascade: a
    /// deleted agent keeps its sessions. Found by its content, so that a
    /// renumber does not break the test.
    #[test]
    fn migration_39_keeps_questions_and_takes_bundles() {
        let mut conn = Connection::open_in_memory().unwrap();
        let n = MIGRATIONS
            .iter()
            .position(|m| m.contains("kind_bundle"))
            .unwrap();
        for (i, sql) in MIGRATIONS.iter().enumerate().take(n) {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", i + 1).unwrap();
        }
        conn.execute(
            "INSERT INTO questions (id, agent, project, text, kind, thread_id, created_at, updated_at)
               VALUES ('Q1', 'A1', 'p', 'Ship?', 'notice', 'T0', 't', 't')",
            [],
        )
        .unwrap();
        migrate(&mut conn).unwrap();
        let (kind, items): (String, Option<String>) = conn
            .query_row(
                "SELECT kind, items FROM questions WHERE id = 'Q1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((kind.as_str(), items), ("notice", None));
        conn.execute(
            "INSERT INTO questions (id, agent, project, text, kind, items, thread_id, created_at,
                                    updated_at)
               VALUES ('Q2', 'A1', 'p', 'Plan', 'bundle', '[]', 'T1', 't', 't')",
            [],
        )
        .unwrap();
        assert!(
            conn.execute("UPDATE questions SET kind = 'bogus' WHERE id = 'Q1'", [])
                .is_err()
        );
    }

    /// Migration 67 keeps each question and its kind, and takes the kind
    /// `tool_run` with its columns. Found by its content.
    #[test]
    fn migration_67_keeps_questions_and_takes_tool_runs() {
        let mut conn = Connection::open_in_memory().unwrap();
        let n = MIGRATIONS
            .iter()
            .position(|m| m.contains("kind_tool_run"))
            .unwrap();
        for (i, sql) in MIGRATIONS.iter().enumerate().take(n) {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", i + 1).unwrap();
        }
        conn.execute(
            "INSERT INTO questions (id, agent, project, text, kind, items, thread_id, created_at,
                                    updated_at)
               VALUES ('Q1', 'A1', 'p', 'Plan', 'bundle', '[]', 'T0', 't', 't')",
            [],
        )
        .unwrap();
        migrate(&mut conn).unwrap();
        let kind: String = conn
            .query_row("SELECT kind FROM questions WHERE id = 'Q1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(kind, "bundle");
        conn.execute(
            "INSERT INTO questions (id, agent, project, text, kind, tool_run, thread_id,
                                    created_at, updated_at)
               VALUES ('Q2', 'A1', 'p', 'Run?', 'tool_run', '{}', 'T1', 't', 't')",
            [],
        )
        .unwrap();
        conn.execute("UPDATE questions SET used_at = 't2' WHERE id = 'Q2'", [])
            .unwrap();
        assert!(
            conn.execute("UPDATE questions SET kind = 'bogus' WHERE id = 'Q1'", [])
                .is_err()
        );
    }

    /// Migration 69 keeps each role and takes `architect`. Found by its
    /// content.
    #[test]
    fn migration_69_keeps_roles_and_takes_architect() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        let n = MIGRATIONS
            .iter()
            .position(|m| m.contains("roles_architect"))
            .unwrap();
        for (i, sql) in MIGRATIONS.iter().enumerate().take(n) {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", i + 1).unwrap();
        }
        conn.execute_batch(
            "INSERT INTO projects (name, created_at, updated_at) VALUES ('p', 't', 't');
             INSERT INTO agents (id, name, project, harness, workspace, created_at, updated_at)
               VALUES ('A1', 'a', 'p', 'claude', '/w', 't', 't');
             INSERT INTO roles (agent_id, role, project, granted_by, granted_at)
               VALUES ('A1', 'sysadmin', NULL, 'operator', 't');",
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO roles (agent_id, role, granted_by, granted_at)
                 VALUES ('A1', 'architect', 'operator', 't')",
                [],
            )
            .is_err()
        );
        migrate(&mut conn).unwrap();
        let roles: Vec<String> = conn
            .prepare("SELECT role FROM roles WHERE agent_id = 'A1'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(roles, ["sysadmin"]);
        conn.execute(
            "INSERT INTO roles (agent_id, role, granted_by, granted_at)
             VALUES ('A1', 'architect', 'operator', 't')",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO roles (agent_id, role, granted_by, granted_at)
                 VALUES ('A1', 'bogus', 'operator', 't')",
                [],
            )
            .is_err()
        );
        // The cascade from agents still works.
        conn.execute("DELETE FROM agents WHERE id = 'A1'", [])
            .unwrap();
        let left: i64 = conn
            .query_row("SELECT count(*) FROM roles", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0);
    }

    #[test]
    fn migration_37_keeps_sessions_of_a_deleted_agent() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        let n = MIGRATIONS
            .iter()
            .position(|m| m.contains("sessions_rebuilt"))
            .unwrap();
        for (i, sql) in MIGRATIONS.iter().enumerate().take(n) {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", i + 1).unwrap();
        }
        conn.execute_batch(
            "INSERT INTO projects (name, created_at, updated_at) VALUES ('p', 't', 't');
             INSERT INTO agents (id, name, project, harness, workspace, created_at, updated_at)
               VALUES ('A1', 'a', 'p', 'claude', '/w', 't', 't');
             INSERT INTO sessions (id, agent_id, harness_session_id, current, started_at,
                                   resumes, resumed_at)
               VALUES ('S1', 'A1', 'h1', 1, 't0', 2, 't1');",
        )
        .unwrap();
        migrate(&mut conn).unwrap();
        let row = |c: &Connection| -> (String, Option<String>, i64, i64, Option<String>) {
            c.query_row(
                "SELECT agent_id, harness_session_id, current, resumes, resumed_at
                 FROM sessions WHERE id = 'S1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap()
        };
        assert_eq!(
            row(&conn),
            ("A1".into(), Some("h1".into()), 1, 2, Some("t1".into()))
        );
        conn.execute("DELETE FROM agents WHERE id = 'A1'", [])
            .unwrap();
        assert_eq!(row(&conn).0, "A1", "no cascade");
        let col: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('card_comments') WHERE name = 'session_id'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(col, 1);
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

    /// S4: a secret value cannot blind a record. The keys of a JSON
    /// object and the identifier fields stay; the free text is redacted,
    /// and the count is right.
    #[test]
    fn redaction_keeps_keys_and_identifier_fields() {
        let me = "01K8AGENT0000000000000000A";
        let r = Redactor::new([
            ("p/x".to_owned(), me),
            ("p/y".to_owned(), "grantee_label"),
            ("p/z".to_owned(), "operator"),
            ("p/t".to_owned(), "ghp_R3alLook1ngT0ken"),
        ])
        .unwrap();
        let detail = serde_json::json!({
            "agent": me, "grantee_label": "agent:q/b", "by": "operator",
            "id": me, "agent_id": me, "note": "token ghp_R3alLook1ngT0ken",
            "nested": {"project_ids": [me], "items": ["ghp_R3alLook1ngT0ken"]},
        });
        let (out, n) = redact_json_as(&r, &View::All, detail);
        assert_eq!(out["agent"], me, "{out}");
        assert_eq!(out["id"], me, "{out}");
        assert_eq!(out["agent_id"], me, "{out}");
        assert_eq!(out["by"], "operator", "{out}");
        assert_eq!(out["grantee_label"], "agent:q/b", "{out}");
        assert_eq!(out["nested"]["project_ids"][0], me, "{out}");
        assert_eq!(out["note"], "token [secret:p/t]", "{out}");
        assert_eq!(out["nested"]["items"][0], "[secret:p/t]", "{out}");
        assert_eq!(n, 2);
        let keys: Vec<&String> = out.as_object().unwrap().keys().collect();
        assert!(keys.iter().all(|k| !k.contains("[secret")), "{keys:?}");
        // The text of an agent (a tool input): each value is free text.
        let input = serde_json::json!({"name": "ghp_R3alLook1ngT0ken"});
        assert_eq!(
            redact_json_values(&r, &View::All, input)["name"],
            "[secret:p/t]"
        );
    }

    fn test_db() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c).unwrap();
        for p in ["p", "q"] {
            c.execute(
                "INSERT INTO projects (name, created_at, updated_at) VALUES (?1, 'x', 'x')",
                [p],
            )
            .unwrap();
        }
        for (id, project, name, roles) in [
            ("01K8AGENT0000000000000000A", "p", "a", "[]"),
            (
                "01K8AGENT0000000000000000B",
                "q",
                "b",
                "[\"project_admin\"]",
            ),
            ("01K8AGENT0000000000000000C", "q", "s", "[\"sysadmin\"]"),
        ] {
            c.execute(
                "INSERT INTO agents (id, project, name, harness, workspace, roles,
                                     created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'claude', '', ?4, 'x', 'x')",
                rusqlite::params![id, project, name, roles],
            )
            .unwrap();
        }
        c
    }

    /// S5: the label of a write shows the names that its actor may see.
    #[test]
    fn the_writer_view_follows_the_actor() {
        let c = test_db();
        assert_eq!(writer_view(&c, "operator"), View::All);
        assert_eq!(writer_view(&c, "system"), View::Hidden);
        assert_eq!(writer_view(&c, "schedule:01K8X"), View::Hidden);
        assert_eq!(
            writer_view(&c, "agent:01K8AGENT0000000000000000A"),
            View::Scope("p".to_owned())
        );
        assert_eq!(
            writer_view(&c, "agent:01K8AGENT0000000000000000B"),
            View::Scope("q".to_owned())
        );
        assert_eq!(
            writer_view(&c, "agent:01K8AGENT0000000000000000C"),
            View::All
        );
        assert_eq!(writer_view(&c, "agent:gone"), View::Hidden);
    }

    const VALUE: &str = "ghp_R3alLook1ngT0kenRace";

    fn put_value(c: &Connection, k: &crate::secrets::MasterKey, scope: &str, name: &str) {
        let input: crate::secrets::SecretInput =
            serde_json::from_value(serde_json::json!({"fields": {"value": VALUE}})).unwrap();
        crate::secrets::put(
            c,
            k,
            crate::secrets::PutMode::Upsert,
            &crate::secrets::NewSecret {
                scope,
                name,
                kind: crate::secrets::Kind::Token,
                input: &input,
                by: "operator",
                strict: true,
            },
        )
        .unwrap();
    }

    fn key_dir() -> (tempfile::TempDir, crate::secrets::MasterKey) {
        let dir = tempfile::tempdir().unwrap();
        let k = crate::secrets::MasterKey::generate();
        crate::secrets::crypto::write_key(&crate::secrets::crypto::key_path(dir.path()), &k)
            .unwrap();
        (dir, k)
    }

    /// S11: a write that starts while a secret put runs stores the label,
    /// never the value. Before the fix, the second write took its matcher
    /// before the lock, and the put built the new matcher after its commit.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_write_during_a_secret_put_never_stores_the_value() {
        let (dir, k) = key_dir();
        let db = Db::new(test_db(), EventHub::default());
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let state_dir = dir.path().to_owned();
        let db1 = db.clone();
        let put = tokio::spawn(async move {
            db1.write("operator", move |t| {
                t.rebuild_redactor(&state_dir);
                put_value(t, &k, crate::secrets::GLOBAL, "gh");
                // Hold the lock until the other write waits for it.
                wait.recv().unwrap();
                Ok(())
            })
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let db2 = db.clone();
        let other = tokio::spawn(async move {
            db2.write("operator", |t| {
                t.audit(
                    "test.leak",
                    "x",
                    None,
                    serde_json::json!({"text": format!("leak {VALUE}")}),
                )?;
                Ok(t.redact_string(format!("mail {VALUE}")))
            })
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        go.send(()).unwrap();
        put.await.unwrap().unwrap();
        let mail = other.await.unwrap().unwrap();
        assert_eq!(mail, "mail [secret:gh]");
        let detail: String = db
            .read(|c| {
                Ok(c.query_row(
                    "SELECT detail FROM audit WHERE action = 'test.leak'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert!(!detail.contains(VALUE), "{detail}");
        assert!(detail.contains("[secret:gh]"), "{detail}");
        assert!(detail.contains("\"redacted\":1"), "{detail}");
    }

    /// S5: a project agent's write shows `[secret]` for a secret outside
    /// its project, and the name of a secret of its own project.
    #[tokio::test]
    async fn a_project_agent_sees_only_its_own_names() {
        let (dir, k) = key_dir();
        let db = Db::new(test_db(), EventHub::default());
        let d = dir.path().to_owned();
        db.write("operator", move |t| {
            t.rebuild_redactor(&d);
            put_value(t, &k, "q", "db");
            Ok(())
        })
        .await
        .unwrap();
        let text = format!("x {VALUE} y");
        for (actor, want) in [
            ("agent:01K8AGENT0000000000000000A", "x [secret] y"),
            ("agent:01K8AGENT0000000000000000B", "x [secret:q/db] y"),
            ("agent:01K8AGENT0000000000000000C", "x [secret:q/db] y"),
            ("system", "x [secret] y"),
            ("operator", "x [secret:q/db] y"),
        ] {
            let t2 = text.clone();
            let got = db
                .write(actor, move |t| Ok(t.redact_string(t2)))
                .await
                .unwrap();
            assert_eq!(got, want, "{actor}");
        }
    }

    #[test]
    fn migration_48_gives_existing_boards_roles_and_fixed_ends() {
        let mut conn = Connection::open_in_memory().unwrap();
        let n = MIGRATIONS
            .iter()
            .position(|m| m.contains("board_columns_role"))
            .unwrap();
        for (i, sql) in MIGRATIONS.iter().enumerate().take(n) {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", i + 1).unwrap();
        }
        // A board whose operator put `done` before `review` and gave the
        // backlog a WIP limit.
        conn.execute_batch(
            "INSERT INTO projects (name, created_at, updated_at) VALUES ('p', 't', 't');
             INSERT INTO boards (project, created_at) VALUES ('p', 't');
             INSERT INTO board_columns (project, name, position, wip_limit) VALUES
               ('p', 'backlog', 0, 5), ('p', 'in_progress', 1, 2), ('p', 'blocked', 2, NULL),
               ('p', 'done', 3, 9), ('p', 'review', 4, NULL);",
        )
        .unwrap();
        migrate(&mut conn).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT name, wip_limit, role FROM board_columns WHERE project = 'p'
                 ORDER BY position",
            )
            .unwrap();
        let rows: Vec<(String, Option<i64>, Option<String>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let want = [
            ("backlog", None, None),
            ("in_progress", Some(2), Some("working")),
            ("blocked", None, Some("blocked")),
            ("review", None, None),
            ("done", None, None),
        ];
        let got: Vec<(&str, Option<i64>, Option<&str>)> = rows
            .iter()
            .map(|(n, w, r)| (n.as_str(), *w, r.as_deref()))
            .collect();
        assert_eq!(got, want);
        // One column for each role.
        let dup = conn.execute(
            "UPDATE board_columns SET role = 'working' WHERE name = 'review'",
            [],
        );
        assert!(dup.is_err());
        // The CHECK on the role is gone since migration 82: the store
        // refuses an unknown role (tests/board.rs).
    }

    /// TER-580: the rounds that the TER-553 upgrade backfilled, filled or
    /// not, need a fresh accept after `ter-580-backfilled-heads.sql`: no
    /// commit, a note, and the background read does not pick them. A round
    /// of a review accept keeps its commit.
    #[test]
    fn backfilled_rounds_need_a_fresh_accept() {
        let mut conn = Connection::open_in_memory().unwrap();
        let (before, after): (Vec<_>, Vec<_>) = NAMED_MIGRATIONS
            .iter()
            .copied()
            .partition(|(n, _)| *n < "ter-553-accepted-heads.sql");
        migrate_with(&mut conn, MIGRATIONS).unwrap();
        migrate_named(&mut conn, &before).unwrap();
        let rest: Vec<_> = after
            .iter()
            .copied()
            .filter(|(n, _)| !n.starts_with("ter-553-") && !n.starts_with("ter-580-"))
            .collect();
        migrate_named(&mut conn, &rest).unwrap();
        conn.execute_batch(
            "INSERT INTO projects (name, created_at, updated_at) VALUES ('p', 't', 't');
             INSERT INTO boards (project, created_at) VALUES ('p', 't');
             INSERT INTO board_columns (project, name, position, role) VALUES
               ('p', 'review', 0, NULL), ('p', 'merge_queue', 1, 'merge_queue');
             INSERT INTO cards (id, project, number, column_id, position, title, body,
                                labels, created_by, created_at, updated_at)
             SELECT 'P-' || n, 'p', n, (SELECT id FROM board_columns WHERE name = 'merge_queue'),
                    n, 't', '', '[]', 'operator', 't', 't'
             FROM (SELECT 1 AS n UNION ALL SELECT 2);",
        )
        .unwrap();
        let ter553: Vec<_> = after
            .iter()
            .copied()
            .filter(|(n, _)| n.starts_with("ter-553-"))
            .collect();
        migrate_named(&mut conn, &ter553).unwrap();
        // The first read after the upgrade filled P-1 with the tip then;
        // P-2 was still open. P-3 came in by a review accept since.
        conn.execute_batch(
            "UPDATE card_accepted_heads SET branch = 'p-1', commit_sha = 'aaaa',
                    recorded_at = 't' WHERE card = 'P-1';
             INSERT INTO cards (id, project, number, column_id, position, title, body,
                                labels, created_by, created_at, updated_at)
             VALUES ('P-3', 'p', 3, (SELECT id FROM board_columns WHERE name = 'merge_queue'),
                     3, 't', '', '[]', 'operator', 't', 't');
             INSERT INTO card_accepted_heads (card, round, accepted_at, branch, commit_sha)
             VALUES ('P-3', '01ROUND', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), 'p-3', 'cccc');",
        )
        .unwrap();
        assert!(migrate(&mut conn).unwrap() >= 1);
        let mut q = conn
            .prepare(
                "SELECT card, commit_sha, needs_accept, note FROM card_accepted_heads
                 ORDER BY card",
            )
            .unwrap();
        let rows: Vec<(String, Option<String>, i64, Option<String>)> = q
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows.len(), 3, "{rows:?}");
        for (card, commit, needs, note) in &rows[..2] {
            assert_eq!((commit.as_deref(), *needs), (None, 1), "{card}: {rows:?}");
            assert!(
                note.as_deref()
                    .unwrap_or_default()
                    .contains("accept the card again")
            );
        }
        assert_eq!((rows[2].1.as_deref(), rows[2].2), (Some("cccc"), 0));
        let pending =
            crate::store::accepted_heads::pending(&conn, "9999", "2000-01-01T00:00:00.000Z")
                .unwrap();
        assert!(pending.is_empty(), "{pending:?}");
    }

    /// Migration 82 (TER-165): the role column takes the job roles. The
    /// roles of migration 48 stay, and the one-column-per-role index stays.
    #[test]
    fn migration_82_takes_the_job_roles() {
        let mut conn = Connection::open_in_memory().unwrap();
        let n = MIGRATIONS
            .iter()
            .position(|m| m.contains("role_v82"))
            .unwrap();
        for (i, sql) in MIGRATIONS.iter().enumerate().take(n) {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", i + 1).unwrap();
        }
        conn.execute_batch(
            "INSERT INTO projects (name, created_at, updated_at) VALUES ('p', 't', 't');
             INSERT INTO boards (project, created_at) VALUES ('p', 't');
             INSERT INTO board_columns (project, name, position, role) VALUES
               ('p', 'backlog', 0, NULL), ('p', 'in_progress', 1, 'working'),
               ('p', 'review', 2, NULL), ('p', 'blocked', 3, 'blocked'), ('p', 'done', 4, NULL);",
        )
        .unwrap();
        migrate(&mut conn).unwrap();
        let mut stmt = conn
            .prepare("SELECT name, role FROM board_columns WHERE project = 'p' ORDER BY position")
            .unwrap();
        let rows: Vec<(String, Option<String>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let got: Vec<(&str, Option<&str>)> = rows
            .iter()
            .map(|(n, r)| (n.as_str(), r.as_deref()))
            .collect();
        assert_eq!(
            got,
            [
                ("backlog", None),
                ("in_progress", Some("working")),
                ("review", None),
                ("blocked", Some("blocked")),
                ("done", None),
            ]
        );
        // A job role now fits, and still only one column has it.
        conn.execute(
            "UPDATE board_columns SET role = 'validating' WHERE name = 'review'",
            [],
        )
        .unwrap();
        let dup = conn.execute(
            "UPDATE board_columns SET role = 'validating' WHERE name = 'done'",
            [],
        );
        assert!(dup.is_err());
    }

    /// Migration 51 (ephemeral agents) as the dev stack applied it on
    /// 2026-11-01: `agents.owner` and the `spawns` table with these
    /// columns. A database at 51 migrates on to the end, and a spawn row
    /// written at 51 reads back.
    #[test]
    fn migration_51_spawns_as_applied() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        let n = MIGRATIONS
            .iter()
            .position(|m| m.contains("CREATE TABLE spawns"))
            .unwrap()
            + 1;
        assert_eq!(n, 51);
        for (i, sql) in MIGRATIONS.iter().enumerate().take(n) {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", i + 1).unwrap();
        }
        let cols = |table: &str| -> Vec<String> {
            let mut st = conn
                .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
                .unwrap();
            st.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert_eq!(
            cols("spawns"),
            [
                "id",
                "owner",
                "owner_name",
                "project",
                "name",
                "template",
                "harness",
                "task",
                "spawned_by",
                "spawned_at",
                "deadline_at",
                "wait_id",
                "finished_at",
                "outcome",
                "result",
                "session_id"
            ]
        );
        assert!(cols("agents").contains(&"owner".to_owned()));
        conn.execute(
            "INSERT INTO spawns (id, owner, owner_name, project, name, template, harness, task,
                                 spawned_by, spawned_at, deadline_at)
             VALUES ('E1', 'O1', 'lead', 'p', 'lead-w-1', 'w', 'claude', 't', 'operator',
                     '2026-11-01T00:00:00.000Z', '2026-11-01T02:00:00.000Z')",
            [],
        )
        .unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), migration_count() - n);
        let name: String = conn
            .query_row("SELECT name FROM spawns WHERE id = 'E1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "lead-w-1");
    }

    /// TER-16: a write holds the write lock from its start. Another
    /// connection to the same file cannot commit between the read and the
    /// write of a transaction (with a DEFERRED transaction it could, and
    /// the write then failed at once with `database is locked`).
    #[tokio::test]
    async fn a_write_holds_the_lock_against_another_connection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let db = Db::new(open(&path).unwrap(), EventHub::default());
        let other = open(&path).unwrap();
        other.execute_batch("CREATE TABLE x (v INTEGER)").unwrap();
        other
            .busy_timeout(std::time::Duration::from_millis(50))
            .unwrap();
        let n = db
            .write("system", move |t| {
                let n: i64 = t.tx.query_row("SELECT count(*) FROM x", [], |r| r.get(0))?;
                // The other connection tries to commit between the read
                // and the write, and cannot.
                let e = other.execute("INSERT INTO x VALUES (1)", []).unwrap_err();
                assert!(e.to_string().contains("locked"), "{e}");
                t.tx.execute("INSERT INTO x VALUES (2)", [])?;
                Ok(n)
            })
            .await
            .unwrap();
        assert_eq!(n, 0);
        let rows: Vec<i64> = open(&path)
            .unwrap()
            .prepare("SELECT v FROM x")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows, vec![2]);
    }

    /// TER-224: an error in a savepoint rolls back its rows and drops its
    /// events. The writes before it and a good savepoint stay.
    #[test]
    fn a_failed_savepoint_keeps_nothing_of_its_writes() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        let mut t = Tx::for_test(conn.transaction().unwrap(), "system");
        let insert = |t: &mut Tx<'_>, name: &str| -> ApiResult<()> {
            t.execute(
                "INSERT INTO projects (name, description, created_at, updated_at)
                 VALUES (?1, '', 'a', 'a')",
                [name],
            )?;
            t.emit(crate::events::EventDraft::new(
                "system",
                name,
                serde_json::json!({}),
            ))?;
            Ok(())
        };
        insert(&mut t, "before").unwrap();
        let r: ApiResult<()> = t.savepoint(|t| {
            insert(t, "lost")?;
            t.rebuild_redactor(Path::new("/state"));
            Err(ApiError::internal("the second step fails"))
        });
        assert!(r.is_err());
        assert_eq!(
            t.rebuild, None,
            "the rebuild of the failed savepoint is undone"
        );
        t.savepoint(|t| insert(t, "kept")).unwrap();
        let names: Vec<String> = t
            .prepare("SELECT name FROM projects ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(names, ["before", "kept"]);
        let summaries: Vec<&str> = t.events.iter().map(|e| e.summary.as_str()).collect();
        assert_eq!(summaries, ["before", "kept"]);
        let stored: i64 = t
            .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, 2);
    }
}
