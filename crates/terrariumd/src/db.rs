//! SQLite storage and schema migrations.
//!
//! Each migration runs once, in order. `PRAGMA user_version` records how many
//! have run. Never edit a migration that has shipped; add a new one.

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
];

/// The shared database handle. One connection behind a mutex; every call
/// runs on the blocking pool, so the async runtime never waits on SQLite.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
    hub: EventHub,
    /// The redaction matcher (PLAN §16b.6). Each write transaction takes a
    /// snapshot of it under the database lock ([`Tx::redact`]). A write
    /// that adds a value builds the new matcher before its commit and swaps
    /// it in before it lets the lock go ([`Tx::rebuild_redactor`]), so no
    /// other write sees the value with the old matcher.
    redactor: RedactorHandle,
}

impl Db {
    pub fn new(conn: Connection, hub: EventHub) -> Self {
        Self {
            conn: Arc::new(Mutex::new(conn)),
            hub,
            redactor: RedactorHandle::default(),
        }
    }

    pub fn hub(&self) -> &EventHub {
        &self.hub
    }

    /// The daemon's redaction matcher. Clones share the same slot.
    pub fn redactor(&self) -> &RedactorHandle {
        &self.redactor
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

    /// Run a read on the calling thread. Only for a short read at the
    /// daemon start, where the caller cannot wait on the blocking pool.
    pub fn read_now<T>(&self, f: impl FnOnce(&Connection) -> ApiResult<T>) -> ApiResult<T> {
        let guard = match self.conn.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&guard)
    }

    /// Run a write in one transaction. `actor` labels the events and audit
    /// rows that the write makes. The transaction commits when `f` returns
    /// `Ok`; then the new events and audit rows go to the subscribers.
    pub async fn write<T, F>(&self, actor: impl Into<String>, f: F) -> ApiResult<T>
    where
        T: Send + 'static,
        F: for<'c> FnOnce(&mut Tx<'c>) -> ApiResult<T> + Send + 'static,
    {
        let conn = self.conn.clone();
        let actor = actor.into();
        let handle = self.redactor.clone();
        let (value, events, audits) = tokio::task::spawn_blocking(move || {
            let mut guard = match conn.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            // The snapshot comes after the lock: a write that changed a
            // secret swapped its matcher in before it let the lock go.
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
                tx,
                actor,
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

/// A write transaction with the actor and the events and audit rows it made.
pub struct Tx<'c> {
    tx: rusqlite::Transaction<'c>,
    /// `operator`, `agent:<ulid>`, `system`, or `schedule:<ulid>` (the
    /// writes of a trigger run).
    pub actor: String,
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
            tx,
            actor: actor.to_owned(),
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
            "test_run_versions",
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
        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len() - 5);
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
        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len() - 6);
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
        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len() - 7);
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
        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len() - 8);
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
        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len() - 23);
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
        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len() - 33);
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
        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len() - 28);
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
        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len() - 29);
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
        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len() - 31);
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
        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len() - 23);
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
        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len() - 30);
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
        let bad = conn.execute(
            "UPDATE board_columns SET role = 'stuck' WHERE name = 'review'",
            [],
        );
        assert!(bad.is_err());
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
        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len() - n);
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
}
