-- No historical intents or successful cleanup receipts are inferred.
CREATE TABLE run_cleanup (
    operation_id TEXT PRIMARY KEY,
    wait_id TEXT NOT NULL UNIQUE REFERENCES waits(id) ON DELETE CASCADE,
    project TEXT NOT NULL,
    owner TEXT NOT NULL,
    requester TEXT NOT NULL,
    session TEXT,
    target TEXT,
    schema_version INTEGER NOT NULL CHECK(schema_version = 1),
    requested_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    phase TEXT NOT NULL CHECK(phase IN ('queued','dispatch_started','finished')),
    status TEXT NOT NULL CHECK(status IN ('pending','unknown','unavailable')),
    reason TEXT NOT NULL,
    diagnostic TEXT CHECK(diagnostic IS NULL OR length(diagnostic) <= 1024),
    CHECK ((phase = 'queued' AND status = 'pending')
        OR (phase = 'dispatch_started' AND status = 'unknown')
        OR (phase = 'finished' AND status IN ('unknown', 'unavailable')))
);
