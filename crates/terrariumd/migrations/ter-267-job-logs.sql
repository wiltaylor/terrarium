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
