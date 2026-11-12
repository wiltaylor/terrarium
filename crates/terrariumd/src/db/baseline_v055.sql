-- Frozen named migration SQL from v0.55.0-alpha in filename order.
ALTER TABLE threads ADD COLUMN agent_end_outcome TEXT;
ALTER TABLE threads ADD COLUMN restart_context_sent INTEGER NOT NULL DEFAULT 0;

CREATE TABLE github_issue_imports (
    project TEXT NOT NULL REFERENCES projects(name) ON DELETE CASCADE,
    url TEXT NOT NULL,
    card TEXT NOT NULL REFERENCES cards(id),
    PRIMARY KEY(project, url)
);

-- Version groups and card labels replaced these objects. Dropping the
-- tables also drops their indexes. This discards legacy metadata only.
DROP TABLE test_run_versions;
DROP TABLE version_reviews;
DROP TABLE version_artifacts;
DROP TABLE version_cards;
DROP TABLE versions;

ALTER TABLE threads ADD COLUMN monitor TEXT;
ALTER TABLE threads ADD COLUMN monitor_done INTEGER NOT NULL DEFAULT 0;
ALTER TABLE thread_pending_wakes RENAME TO thread_pending_wakes_old;
CREATE TABLE thread_pending_wakes (
    thread TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    reason TEXT NOT NULL CHECK(reason IN ('blocked', 'review', 'monitor_green', 'monitor_red')),
    PRIMARY KEY(thread, reason)
);
INSERT INTO thread_pending_wakes SELECT * FROM thread_pending_wakes_old;
DROP TABLE thread_pending_wakes_old;

-- Keep history reads bounded to card moves even in busy projects. Some older
-- events have non-JSON payloads, so index evaluation must guard json_extract.
CREATE INDEX events_card_moves ON events(project, seq)
    WHERE kind = 'card'
      AND CASE WHEN json_valid(payload) THEN json_extract(payload, '$.change') END = 'moved';

CREATE TABLE resource_budgets (
    host TEXT PRIMARY KEY REFERENCES hosts(name) ON DELETE CASCADE,
    percentage INTEGER NOT NULL CHECK (percentage BETWEEN 0 AND 100),
    io TEXT NOT NULL DEFAULT '{}'
);
CREATE TABLE reservations (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    host TEXT NOT NULL REFERENCES hosts(name),
    project TEXT NOT NULL,
    state TEXT NOT NULL,
    data TEXT NOT NULL
);
CREATE INDEX reservations_host_state ON reservations(host, state, seq);
DROP INDEX waits_open;
ALTER TABLE waits ADD COLUMN kind_resources TEXT NOT NULL DEFAULT 'time'
    CHECK (kind_resources IN ('agent', 'process', 'time', 'card', 'question', 'reservation'));
UPDATE waits SET kind_resources = kind;
ALTER TABLE waits DROP COLUMN kind;
ALTER TABLE waits RENAME COLUMN kind_resources TO kind;
CREATE INDEX waits_open ON waits(state, kind);

ALTER TABLE group_steps ADD COLUMN started_at TEXT;
ALTER TABLE group_steps ADD COLUMN finished_at TEXT;
CREATE INDEX group_steps_history ON group_steps(name, state, group_card)
    WHERE started_at IS NOT NULL AND finished_at IS NOT NULL;

CREATE TABLE conflict_fixes (
    -- Keep history across the later jobs-table rebuilds.
    job TEXT NOT NULL,
    card TEXT NOT NULL REFERENCES cards(id),
    project TEXT NOT NULL REFERENCES projects(name),
    template TEXT NOT NULL,
    task TEXT NOT NULL,
    done INTEGER NOT NULL DEFAULT 0,
    retry_at INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    PRIMARY KEY (job, card)
);
CREATE INDEX conflict_fixes_pending ON conflict_fixes(done, retry_at);
CREATE INDEX conflict_fixes_card_pending ON conflict_fixes(card, done);
-- The ready-card query orders equal positions by the card's TEXT id,
-- not rowid. Cover that tie-breaker without a temporary sort.
CREATE INDEX cards_dispatch_order ON cards(column_id, position, id);

-- A NULL answer records a started write. Never replay it after a crash.
CREATE TABLE agent_rpc_answers (
    agent TEXT NOT NULL,
    request_id TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    answer TEXT,
    PRIMARY KEY (agent, request_id)
);

-- The host reconnect journal. Payloads include bearer-like resume tokens
-- and command output, so they use the same encryption as secret rows.
CREATE TABLE host_resume (
    id TEXT PRIMARY KEY NOT NULL,
    sealed BLOB NOT NULL
);

CREATE TABLE job_log_budgets (
    project TEXT PRIMARY KEY,
    bytes INTEGER NOT NULL DEFAULT 0,
    reconcile_cursor INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE job_logs (
    id INTEGER PRIMARY KEY,
    project TEXT NOT NULL,
    job TEXT NOT NULL,
    attempt INTEGER NOT NULL,
    target TEXT NOT NULL,
    path TEXT NOT NULL,
    root TEXT NOT NULL DEFAULT '',
    root_identity TEXT NOT NULL DEFAULT '',
    file_identity TEXT NOT NULL DEFAULT '',
    target_identity TEXT NOT NULL DEFAULT '',
    written_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    bytes INTEGER NOT NULL DEFAULT 0,
    expired INTEGER NOT NULL DEFAULT 0,
    cleanup_state TEXT NOT NULL DEFAULT 'retained',
    quarantine TEXT NOT NULL DEFAULT '',
    UNIQUE(job, attempt)
);
CREATE INDEX job_logs_cleanup ON job_logs(project, cleanup_state, id);

-- TER-294: the target kind `exedev`. A CHECK cannot change in place, so
-- the column is swapped, as in legacy migration 25.
ALTER TABLE targets ADD COLUMN kind_exedev TEXT NOT NULL DEFAULT 'host'
    CHECK (kind_exedev IN ('host', 'ssh', 'docker', 'vmlab', 'vnc', 'rdp', 'desktop', 'exedev'));
UPDATE targets SET kind_exedev = kind;
ALTER TABLE targets DROP COLUMN kind;
ALTER TABLE targets RENAME COLUMN kind_exedev TO kind;

-- TER-294: the exe.dev VMs that Terrarium made. A row is written before
-- `new` runs and outlives its target, so the sweep finds the VM on the
-- account it was made on (the token secret `api_token` as the target's
-- project `project` opened it, with `admin_set`).
CREATE TABLE exedev_vms (
    name TEXT PRIMARY KEY,
    target TEXT NOT NULL,
    project TEXT,
    api_token TEXT NOT NULL,
    admin_set INTEGER NOT NULL DEFAULT 0,
    tag TEXT NOT NULL,
    ssh_dest TEXT,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL
);
CREATE INDEX exedev_vms_target ON exedev_vms(target);

-- Last observed remote head and the working-time baseline for no-progress.
-- `head` is NULL while the branch is missing on the remote. `observed_at` is
-- when the current head was first seen, `checked_at` the last successful read.
CREATE TABLE card_branch_heads (
    card TEXT PRIMARY KEY REFERENCES cards(id) ON DELETE CASCADE,
    repo TEXT NOT NULL,
    branch TEXT NOT NULL,
    head TEXT,
    working_seconds INTEGER NOT NULL,
    observed_at TEXT NOT NULL,
    checked_at TEXT NOT NULL
);
-- The remote scan walks the working and research columns, then each column's
-- repo cards in id order from a rotating cursor.
CREATE INDEX board_columns_role_watch ON board_columns(role);
CREATE INDEX board_columns_name_watch ON board_columns(lower(name));
CREATE INDEX cards_branch_watch ON cards(column_id, id)
    WHERE archived = 0 AND repo IS NOT NULL;

ALTER TABLE projects ADD COLUMN review_dispatcher TEXT;
CREATE TABLE card_reviews (
    card TEXT PRIMARY KEY REFERENCES cards(id) ON DELETE CASCADE,
    round INTEGER NOT NULL DEFAULT 0,
    agent TEXT,
    cap_notified INTEGER NOT NULL DEFAULT 0,
    -- 0: waiting for verdict, 1: invalidated by move, 2: verdict consumed.
    applied INTEGER NOT NULL DEFAULT 0
);
CREATE TRIGGER card_review_reset AFTER UPDATE OF column_id ON cards
WHEN NEW.column_id <> OLD.column_id
BEGIN
    UPDATE card_reviews SET applied = 1 WHERE card = NEW.id;
END;

-- The subscription session list (store::session_refs::on_subscription)
-- looks up usage by agent, session and subscription for each session.
-- Without this index both lookups walked every usage sample of the
-- subscription per session and held the database lock for seconds.
CREATE INDEX usage_samples_session ON usage_samples(agent, session, subscription);

CREATE INDEX jobs_project_created_at ON jobs(project, created_at);

-- Start-of-day schedules (TER-450, GitHub #29). A schedule keeps the
-- local day of its last start-of-day run (one run per day) and the last
-- detected start (JSON: cause, at). A run gets the trigger 'day_start'
-- and the detected start that planned it. SQLite cannot change a CHECK,
-- so `schedule_runs` is made again, as legacy migration 20 did.
ALTER TABLE schedules ADD COLUMN day_start_done TEXT;
ALTER TABLE schedules ADD COLUMN day_start_last TEXT;
ALTER TABLE schedule_runs RENAME TO schedule_runs_v450;
CREATE TABLE schedule_runs (
    id          TEXT PRIMARY KEY,
    schedule    TEXT NOT NULL REFERENCES schedules(id) ON DELETE CASCADE,
    trigger     TEXT NOT NULL CHECK (trigger IN ('cron', 'manual', 'catch_up', 'event', 'day_start')),
    status      TEXT NOT NULL CHECK (status IN ('running', 'ok', 'failed', 'skipped')),
    started_at  TEXT NOT NULL,
    finished_at TEXT,
    summary     TEXT NOT NULL DEFAULT '',
    output      TEXT NOT NULL DEFAULT '{}',
    error       TEXT,
    actor       TEXT NOT NULL,
    event_seq   INTEGER,
    day_start   TEXT
);
INSERT INTO schedule_runs (id, schedule, trigger, status, started_at, finished_at, summary, output, error, actor, event_seq)
    SELECT id, schedule, trigger, status, started_at, finished_at, summary, output, error, actor, event_seq FROM schedule_runs_v450;
DROP TABLE schedule_runs_v450;
CREATE INDEX schedule_runs_schedule ON schedule_runs(schedule, id);
-- The start-of-day pass reads the `day_start` schedules each tick.
CREATE INDEX schedules_spec_kind ON schedules(json_extract(spec, '$.kind'), enabled);

-- GitHub #30 (TER-469): the operator moves an open question or notice into
-- an Inbox thread with its agent. The question gets the status `in_thread`:
-- out of the open count, and answered, acknowledged, or retracted from the
-- thread. The status column is made again with the new CHECK, as legacy
-- migration 30 did; its indexes go first. `threads.from_question` is the
-- question that the thread was opened from.
DROP INDEX questions_agent;
DROP INDEX questions_status;
ALTER TABLE questions ADD COLUMN status_v469 TEXT NOT NULL DEFAULT 'open'
    CHECK (status_v469 IN ('open', 'in_thread', 'answered', 'acknowledged', 'retracted'));
UPDATE questions SET status_v469 = status;
ALTER TABLE questions DROP COLUMN status;
ALTER TABLE questions RENAME COLUMN status_v469 TO status;
CREATE INDEX questions_agent ON questions(agent, status);
CREATE INDEX questions_status ON questions(status, project);
ALTER TABLE threads ADD COLUMN from_question TEXT;

-- Remote observation now watches only working-role columns, served by
-- board_columns_role_watch. Project-scoped name lookups use UNIQUE(project, name).
DROP INDEX board_columns_name_watch;

-- The reviewed commit of each card in the merge queue (TER-553). A move
-- into the merge queue opens the row with no commit; the daemon then reads
-- the head of the card branch on its remote and records it. A move out of
-- the merge queue removes the row. A merge job takes the recorded commit.
CREATE TABLE card_accepted_heads (
    card TEXT PRIMARY KEY REFERENCES cards(id) ON DELETE CASCADE,
    -- A new ULID for each move into the queue: a late read of an earlier
    -- round does not fill a later one.
    round TEXT NOT NULL,
    accepted_at TEXT NOT NULL,
    branch TEXT,
    commit_sha TEXT,
    recorded_at TEXT,
    -- Why the head could not be read yet (no branch on the remote), and
    -- when the read last failed: the next read waits a while.
    note TEXT,
    tried_at TEXT
);
-- The cards that are in the merge queue at the upgrade get a round too, so
-- the queue goes on: the daemon records their head at its first read.
INSERT INTO card_accepted_heads (card, round, accepted_at)
SELECT c.id, 'upgrade-' || c.id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
FROM cards c JOIN board_columns b ON b.id = c.column_id
WHERE c.archived = 0
  AND (b.role = 'merge_queue' OR (b.role IS NULL AND b.name = 'merge_queue'));

-- The time of the admission (unix seconds). The prune timer deletes answers
-- past `agent_rpc::ANSWER_MAX_AGE` in bounded batches on this index. Rows
-- from before this migration count from the upgrade, so a retry across the
-- upgrade still replays.
ALTER TABLE agent_rpc_answers ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0;
UPDATE agent_rpc_answers SET created_at = CAST(strftime('%s', 'now') AS INTEGER);
CREATE INDEX agent_rpc_answers_created ON agent_rpc_answers(created_at);

-- TER-286: keep admission keys forever, but bound retained response payloads.
ALTER TABLE agent_rpc_answers ADD COLUMN compacted INTEGER NOT NULL DEFAULT 0;
CREATE INDEX agent_rpc_answers_retained ON agent_rpc_answers(created_at)
    WHERE compacted = 0;

-- TER-580: a round that the TER-553 upgrade backfilled was not opened by a
-- review accept, so the tip of its branch at the upgrade (or at the first
-- read after it) is not a reviewed commit. Mark those rounds: the
-- background read never fills them, and a merge takes the card only after
-- a fresh accept (a move out of the merge queue and back in).
ALTER TABLE card_accepted_heads ADD COLUMN needs_accept INTEGER NOT NULL DEFAULT 0;
-- The daemon posts one card comment on a round that needs a fresh accept
-- or that the background read may no longer fill, then sets this.
ALTER TABLE card_accepted_heads ADD COLUMN stall_noted INTEGER NOT NULL DEFAULT 0;
UPDATE card_accepted_heads
SET needs_accept = 1, branch = NULL, commit_sha = NULL, recorded_at = NULL,
    note = 'the round was opened by the upgrade, not by a review accept: accept the card again',
    tried_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
WHERE round LIKE 'upgrade-%';

-- TER-607: the branch that a merge job merges onto and pushes. NULL is
-- `next`; `card merge --onto release/X.Y.Z` sets a release branch.
ALTER TABLE jobs ADD COLUMN onto TEXT;

-- Link keys (TER-61, wiki design/linked-instances D2 and D3): another
-- Terrarium daemon (the home) calls this one with a long-lived `tlk_` key.
-- Only SHA-256 hashes of the key and of the one-time `tlp_` pairing code
-- are stored. A revoked row stays, without hashes, so that audit rows with
-- the actor `link:<id>` keep their label.
CREATE TABLE links (
    id TEXT PRIMARY KEY,
    label TEXT NOT NULL,
    read_only INTEGER NOT NULL DEFAULT 0,
    key_hash TEXT UNIQUE,
    pair_code_hash TEXT UNIQUE,
    pair_expires_at TEXT,
    home_instance_id TEXT,
    home_name TEXT,
    paired_at TEXT,
    paired_from TEXT,
    created_by TEXT NOT NULL,
    created_at TEXT NOT NULL,
    rotated_at TEXT,
    first_used_at TEXT,
    last_used_at TEXT,
    last_used_from TEXT,
    revoked_at TEXT
);

-- The review rounds of a card without a review dispatcher (TER-614 R9):
-- each move of the card into a review column. A card of a project with a
-- review dispatcher shows the dispatcher's round (card_reviews) instead.
ALTER TABLE cards ADD COLUMN review_entries INTEGER NOT NULL DEFAULT 0;
-- The moves of the past, from the card events of the log.
UPDATE cards SET review_entries = (
    SELECT count(*) FROM events e
    WHERE e.card = cards.id AND e.kind = 'card'
      AND json_valid(e.payload)
      AND json_extract(e.payload, '$.change') = 'moved'
      AND (json_extract(e.payload, '$.to_role') = 'review'
           OR json_extract(e.payload, '$.to') = 'review'));
CREATE TRIGGER card_review_entry AFTER UPDATE OF column_id ON cards
WHEN NEW.column_id <> OLD.column_id
  AND EXISTS (SELECT 1 FROM board_columns k WHERE k.id = NEW.column_id
              AND (k.role = 'review' OR (k.role IS NULL AND k.name = 'review')))
BEGIN
    UPDATE cards SET review_entries = review_entries + 1 WHERE id = NEW.id;
END;

CREATE TABLE remotes (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    url TEXT NOT NULL,
    instance_id TEXT NOT NULL,
    pin_sha256 TEXT,
    ca_pem TEXT,
    label TEXT NOT NULL,
    sealed BLOB NOT NULL,
    read_only INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    last_ok_at TEXT,
    last_error TEXT,
    status TEXT NOT NULL DEFAULT 'offline',
    calls_day TEXT,
    calls_today INTEGER NOT NULL DEFAULT 0
);

-- TER-623: the `release` job kind. A release job runs the release recipe
-- on the release client target of its project, on a commit of `main`.
-- SQLite cannot change a CHECK, so `jobs` is made again with all of its
-- columns (legacy migrations 71, 72, the batch, planned and slot columns,
-- and ter-607-job-onto) and all of its indexes (also ter-401-job-stats-window,
-- which runs before this file).
ALTER TABLE jobs RENAME TO jobs_v623;
CREATE TABLE jobs (
    id           TEXT PRIMARY KEY,
    project      TEXT NOT NULL,
    card         TEXT NOT NULL,
    kind         TEXT NOT NULL CHECK (kind IN ('validate', 'merge', 'release')),
    repo         TEXT NOT NULL,
    branch       TEXT NOT NULL,
    commit_sha   TEXT,
    state        TEXT NOT NULL DEFAULT 'queued'
        CHECK (state IN ('queued', 'running', 'passed', 'failed', 'cancelled')),
    attempt      INTEGER NOT NULL DEFAULT 1,
    outcome      TEXT,
    exit_code    INTEGER,
    tail         TEXT,
    head         TEXT,
    by_agent     TEXT,
    created_by   TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    started_at   TEXT,
    ended_at     TEXT,
    host         TEXT,
    batch        TEXT,
    parent       TEXT,
    split_path   TEXT NOT NULL DEFAULT '',
    planned_secs INTEGER,
    slot         INTEGER,
    onto         TEXT
);
INSERT INTO jobs (id, project, card, kind, repo, branch, commit_sha, state, attempt, outcome,
                  exit_code, tail, head, by_agent, created_by, created_at, started_at, ended_at,
                  host, batch, parent, split_path, planned_secs, slot, onto)
    SELECT id, project, card, kind, repo, branch, commit_sha, state, attempt, outcome,
           exit_code, tail, head, by_agent, created_by, created_at, started_at, ended_at,
           host, batch, parent, split_path, planned_secs, slot, onto
    FROM jobs_v623;
DROP TABLE jobs_v623;
CREATE INDEX jobs_project_state ON jobs(project, state, created_at);
CREATE INDEX jobs_card ON jobs(card);
CREATE INDEX jobs_host_state ON jobs(host, state);
CREATE INDEX IF NOT EXISTS jobs_project_created_at ON jobs(project, created_at);

-- TER-623 (review-ter-623-r3 finding 2): the release targets that the
-- protection of a release target covers, kept apart from the delivery
-- config. Each set of a project's `release_target` adds the target and its
-- machine (`store::jobs::machine_key`); a switch or a clear removes
-- nothing. Only the operator retires an entry (`DELETE
-- /api/release-targets/{name}`). The rows of the configs set so far come
-- in with no machine: the check also reads the target's machine now.
CREATE TABLE release_targets (
    target   TEXT NOT NULL,
    machine  TEXT NOT NULL DEFAULT '',
    project  TEXT NOT NULL,
    added_by TEXT NOT NULL,
    added_at TEXT NOT NULL,
    PRIMARY KEY (target, machine)
);
INSERT OR IGNORE INTO release_targets (target, machine, project, added_by, added_at)
    SELECT json_extract(def, '$.release_target'), '', project, updated_by, updated_at
    FROM project_delivery
    WHERE coalesce(json_extract(def, '$.release_target'), '') != '';

-- Cloud subscriptions for Claude Code (TER-630, `terrarium_core::cloud`).
-- `cloud` is the JSON of `CloudSettings` (no secret): region, project,
-- resource, base URL, model map, and monthly budget in USD.
ALTER TABLE subscriptions ADD COLUMN cloud TEXT CHECK (cloud IS NULL OR json_valid(cloud));

-- The running `costUSD` total of each Claude Code session, one row per
-- session and UTC month. `base_usd` is the total at the end of the months
-- before, so the spend of the month is `total_usd - base_usd`. A tail that
-- reads a session file again writes the same totals: nothing counts twice.
CREATE TABLE cloud_spend (
    agent        TEXT NOT NULL,
    session      TEXT NOT NULL,
    month        TEXT NOT NULL,
    subscription TEXT,
    base_usd     REAL NOT NULL DEFAULT 0,
    total_usd    REAL NOT NULL DEFAULT 0,
    updated_at   TEXT NOT NULL,
    PRIMARY KEY (agent, session, month)
);
CREATE INDEX cloud_spend_sub ON cloud_spend(subscription, month);

-- The agents on a subscription, for the budget hold under the write lock
-- (`store::cloud_spend::agents_on`, TER-394: an indexed query).
CREATE INDEX agents_subscription ON agents(subscription);

-- TER-653: the exe.dev instance id of this daemon. It is in the name
-- prefix of each VM that this daemon makes
-- (`terrarium-x-<id>-<target>-<tail>`), so the sweep of one daemon never
-- takes a VM of another daemon on the same exe.dev account for a leftover.
-- The daemon writes the one row at its first exe.dev use, not this file:
-- a copy of a fresh database (a test template) gets its own id.
CREATE TABLE exedev_instance (
    id TEXT NOT NULL
);

-- Card pages read linked question history, in the Inbox's display order.
CREATE INDEX questions_card_order ON questions(
    card,
    (status = 'open') DESC,
    CASE WHEN status = 'open' THEN
        CASE priority WHEN 'critical' THEN 2 WHEN 'urgent' THEN 1 ELSE 0 END
        ELSE 0 END DESC,
    CASE WHEN status = 'open' THEN created_at END ASC,
    coalesce(answered_at, updated_at) DESC,
    id
);

ALTER TABLE threads ADD COLUMN request INTEGER NOT NULL DEFAULT 0;
ALTER TABLE threads ADD COLUMN request_epoch INTEGER NOT NULL DEFAULT 0;

CREATE INDEX threads_request_close ON threads(request, status, updated_at, id)
WHERE request = 1 AND status = 'waiting_operator' AND close_proposal IS NOT NULL;

-- Default Inbox pages and their badge share this actionable-row predicate.
-- Cover the page keys and counts without reading bodies of excluded notices.
CREATE INDEX questions_default_inbox ON questions(project, priority, created_at, kind, id)
WHERE status = 'open' AND thread IS NULL
  AND (kind IN ('question', 'bundle', 'tool_run') OR (kind = 'notice' AND priority = 'critical'));

-- Run after TER-469 rebuilds status and TER-688 adds the default index.
-- Cover filtered Inbox candidate pages without reading question bodies.
CREATE INDEX questions_inbox_pages ON questions(
    (status = 'open') DESC,
    (status = 'open' AND priority = 'critical') DESC,
    (status = 'open' AND priority IN ('urgent', 'critical')) DESC,
    created_at DESC, (kind || ':' || id) DESC,
    project, kind, status, id
) WHERE thread IS NULL;

-- Seek directly to a card's newest move, without residual history filtering.
CREATE INDEX events_home_card_moves ON events(card, seq)
    WHERE kind = 'card'
      AND CASE WHEN json_valid(payload) THEN json_extract(payload, '$.change') END = 'moved';

ALTER TABLE projects ADD COLUMN home_board_columns TEXT;
CREATE INDEX cards_home ON cards(archived, id);
CREATE INDEX cards_home_project ON cards(project, archived, id);
CREATE INDEX monitors_home_project ON monitors(project, id);
CREATE INDEX pipelines_home_project ON pipelines(project, id);
CREATE INDEX pipeline_runs_home_latest ON pipeline_runs(pipeline, sort_at DESC, ord ASC, pk DESC);

CREATE TABLE read_positions (
    owner TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('thread', 'mail', 'card')),
    item TEXT NOT NULL,
    read_at TEXT NOT NULL,
    PRIMARY KEY (owner, kind, item)
) WITHOUT ROWID;

ALTER TABLE questions ADD COLUMN multi_select INTEGER NOT NULL DEFAULT 0;
ALTER TABLE questions ADD COLUMN answer_options TEXT NOT NULL DEFAULT '[]';

-- Preserve historical jobs while extending the job-kind CHECK constraint.
ALTER TABLE jobs ADD COLUMN kind_e2e TEXT NOT NULL DEFAULT 'validate'
    CHECK (kind_e2e IN ('validate', 'merge', 'release', 'e2e'));
UPDATE jobs SET kind_e2e = kind;
ALTER TABLE jobs DROP COLUMN kind;
ALTER TABLE jobs RENAME COLUMN kind_e2e TO kind;

-- Opt-in ready-card dispatcher. NULL keeps existing projects disabled.
ALTER TABLE projects ADD COLUMN card_dispatcher TEXT;

CREATE TABLE card_dispatch_retries (
    card TEXT PRIMARY KEY REFERENCES cards(id) ON DELETE CASCADE,
    failures INTEGER NOT NULL,
    retry_at INTEGER NOT NULL
);

CREATE TABLE card_dispatch_deferrals (
    project TEXT PRIMARY KEY REFERENCES projects(name) ON DELETE CASCADE,
    reason TEXT NOT NULL
);

-- Cover API moves, rule moves, archives, and direct transactional updates.
CREATE TRIGGER card_dispatch_retry_cleanup AFTER UPDATE OF column_id, archived ON cards
WHEN NEW.archived = 1 OR
     (SELECT coalesce(role, name) FROM board_columns WHERE id = NEW.column_id) <> 'ready'
BEGIN
    DELETE FROM card_dispatch_retries WHERE card = NEW.id;
END;

-- No agent FK: failed cleanup must survive agent removal and daemon restart.
CREATE TABLE agent_target_roots (
    agent TEXT NOT NULL,
    project TEXT NOT NULL,
    name TEXT NOT NULL,
    target TEXT NOT NULL,
    definition TEXT NOT NULL,
    last_error TEXT,
    PRIMARY KEY (agent, target)
);
CREATE TABLE agent_target_root_warnings (
    agent TEXT NOT NULL,
    target TEXT NOT NULL,
    PRIMARY KEY (agent, target)
);

-- Named to sort after ter-78-target-roots.sql, which makes the table.
-- The base of an owned root when it is not the base of the definition: the
-- home fallback of a target without work_root (TER-498). NULL: the definition.
ALTER TABLE agent_target_roots ADD COLUMN base TEXT;

-- Definition edits invalidate one target's root plans, never scan other targets.
CREATE INDEX agent_target_roots_target ON agent_target_roots(target);
CREATE INDEX agent_target_root_warnings_target ON agent_target_root_warnings(target);

-- Durable once-per-card trouble notices and project thresholds.
CREATE TABLE card_trouble (
    card TEXT NOT NULL REFERENCES cards(id) ON DELETE CASCADE,
    signal TEXT NOT NULL,
    active INTEGER NOT NULL,
    first_at TEXT NOT NULL,
    PRIMARY KEY(card, signal)
);
CREATE TABLE project_trouble (
    project TEXT PRIMARY KEY REFERENCES projects(name) ON DELETE CASCADE,
    settings TEXT NOT NULL CHECK(json_valid(settings))
);

-- Both post-entry history and the single status immediately before entry
-- must seek by assignee and sequence, never walk an assignee's lifetime.
CREATE INDEX events_trouble_agent ON events(agent_id, kind, seq)
    WHERE kind IN ('status', 'wait', 'agent.reset');

-- Do not page leads for the pre-deploy history of existing cards.
CREATE TABLE card_trouble_baseline (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    seq INTEGER NOT NULL
);
INSERT INTO card_trouble_baseline VALUES (1, (SELECT coalesce(max(seq), 0) FROM events));

-- Helper history survives removal of agents and spawn rows.
CREATE TABLE research_helpers (
    id TEXT PRIMARY KEY,
    card TEXT NOT NULL,
    project TEXT NOT NULL,
    spawned_at TEXT NOT NULL,
    finished_at TEXT
);
CREATE INDEX research_helpers_project ON research_helpers(project, spawned_at);
CREATE TABLE research_helper_alerts (card TEXT PRIMARY KEY);

-- A durable, per-attempt receipt also makes upgrade recovery idempotent.
CREATE TABLE request_fallbacks (
    thread TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    epoch INTEGER NOT NULL,
    mail TEXT NOT NULL,
    PRIMARY KEY (thread, epoch)
);

CREATE INDEX jobs_legacy_log_page ON jobs(project, id);
CREATE TABLE legacy_log_listings (
    digest TEXT PRIMARY KEY,
    project TEXT NOT NULL,
    target TEXT NOT NULL,
    target_identity TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    entries TEXT NOT NULL,
    consumed INTEGER NOT NULL DEFAULT 0
);
ALTER TABLE job_logs ADD COLUMN legacy_size INTEGER;
ALTER TABLE job_logs ADD COLUMN legacy_mtime TEXT;

ALTER TABLE remotes ADD COLUMN artifact_origin TEXT;
ALTER TABLE remotes ADD COLUMN artifact_pin_sha256 TEXT;
