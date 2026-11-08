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
