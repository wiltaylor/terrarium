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
