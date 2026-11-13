-- TER-1123: `pending_pointers` keeps `reservation` pointer items too
-- (`Pointer::stored_items` writes them), so that a parked reservation
-- pointer lives through a daemon restart. SQLite cannot change a CHECK,
-- so the table of legacy migration 15 is made again with all of its
-- columns and rows. Its only index is the UNIQUE constraint; no other
-- migration adds one, and no table references it.
ALTER TABLE pending_pointers RENAME TO pending_pointers_v1123;
CREATE TABLE pending_pointers (
    id         INTEGER PRIMARY KEY,
    agent      TEXT NOT NULL,               -- agent ULID
    kind       TEXT NOT NULL CHECK (kind IN ('card', 'lead', 'lease', 'reservation')),
    key        TEXT NOT NULL,
    line       TEXT NOT NULL,
    data       TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE (agent, kind, key)
);
INSERT INTO pending_pointers (id, agent, kind, key, line, data, created_at)
    SELECT id, agent, kind, key, line, data, created_at FROM pending_pointers_v1123;
DROP TABLE pending_pointers_v1123;
