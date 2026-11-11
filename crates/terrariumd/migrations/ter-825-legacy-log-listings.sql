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
