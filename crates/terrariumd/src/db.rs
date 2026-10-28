//! SQLite storage and schema migrations.
//!
//! Each migration runs once, in order. `PRAGMA user_version` records how many
//! have run. Never edit a migration that has shipped; add a new one.

use std::borrow::Cow;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use rusqlite::Connection;
use terrarium_core::redact::{Redactor, RedactorHandle};

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
];

/// The shared database handle. One connection behind a mutex; every call
/// runs on the blocking pool, so the async runtime never waits on SQLite.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
    hub: EventHub,
    /// The redaction matcher (PLAN §16b.6). Each write transaction takes a
    /// snapshot of it ([`Tx::redact`]); `crate::secrets::redact` swaps it
    /// after each change of a secret or a subscription key.
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
    /// `Ok`; then the new events go to the subscribers.
    pub async fn write<T, F>(&self, actor: impl Into<String>, f: F) -> ApiResult<T>
    where
        T: Send + 'static,
        F: for<'c> FnOnce(&mut Tx<'c>) -> ApiResult<T> + Send + 'static,
    {
        let conn = self.conn.clone();
        let actor = actor.into();
        let redactor = self.redactor.get();
        let (value, events) = tokio::task::spawn_blocking(move || {
            let mut guard = match conn.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            let tx = guard.transaction()?;
            let mut t = Tx {
                tx,
                actor,
                cause: None,
                events: Vec::new(),
                redactor,
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
    /// `operator`, `agent:<ulid>`, `system`, or `schedule:<ulid>` (the
    /// writes of a trigger run).
    pub actor: String,
    /// When set, every event of the transaction gets it as `data.cause`
    /// (a trigger run: `{schedule, run, event, depth}`).
    pub cause: Option<serde_json::Value>,
    events: Vec<StoredEvent>,
    /// The redaction matcher at the start of the transaction.
    redactor: Arc<Redactor>,
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
            redactor: Arc::new(Redactor::empty()),
        }
    }

    /// Commit a test transaction (the events stay in it).
    #[cfg(test)]
    pub fn commit_for_test(self) -> rusqlite::Result<()> {
        self.tx.commit()
    }

    /// Replace each known secret in `s` with `[secret:<label>]` (PLAN
    /// §16b.6). Borrows `s` when nothing matches. Each store function that
    /// writes a free text (a mail body, a card, a wiki page) calls it on the
    /// text before the write, so the database never holds the value.
    pub fn redact<'a>(&self, s: &'a str) -> Cow<'a, str> {
        self.redactor.redact_str(s)
    }

    /// [`Self::redact`] of an optional text.
    pub fn redact_opt<'a>(&self, s: Option<&'a str>) -> Option<Cow<'a, str>> {
        s.map(|s| self.redact(s))
    }

    /// [`Self::redact`] of an owned text: the same allocation when nothing
    /// matches.
    pub fn redact_string(&self, s: String) -> String {
        self.redactor.redact_string(s)
    }

    /// [`Self::redact`] of each string in a JSON value.
    pub fn redact_json(&self, v: serde_json::Value) -> serde_json::Value {
        redact_json(&self.redactor, v)
    }

    /// Append an event to the log. Subscribers get it after the commit.
    /// The summary and the data are redacted first, so the event log, the
    /// streams, and the trigger payloads hold no known secret.
    pub fn emit(&mut self, mut draft: EventDraft) -> ApiResult<StoredEvent> {
        if let (Some(c), serde_json::Value::Object(m)) = (&self.cause, &mut draft.data) {
            m.entry("cause").or_insert_with(|| c.clone());
        }
        if !self.redactor.is_empty() {
            draft.summary = self
                .redactor
                .redact_string(std::mem::take(&mut draft.summary));
            draft.data = redact_json(&self.redactor, std::mem::take(&mut draft.data));
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
        let detail = self.redact_json(detail);
        self.tx.execute(
            "UPDATE audit SET detail = ?2, at = ?3 WHERE id = ?1",
            rusqlite::params![id, detail.to_string(), now()],
        )?;
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
        let detail = self.redact_json(detail);
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
                now()
            ],
        )?;
        Ok(())
    }
}

/// Redact each string of a JSON value, object keys too (PLAN §16b.6).
/// Returns the value as it is when the matcher is empty.
pub fn redact_json(r: &Redactor, v: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    if r.is_empty() {
        return v;
    }
    match v {
        Value::String(s) => Value::String(r.redact_string(s)),
        Value::Array(a) => Value::Array(a.into_iter().map(|v| redact_json(r, v)).collect()),
        Value::Object(m) => Value::Object(
            m.into_iter()
                .map(|(k, v)| (r.redact_string(k), redact_json(r, v)))
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
