-- A Grok Build login session (TER-1191, TER-1205): `login_sessions.kind`
-- takes 'grok'. SQLite cannot change a CHECK, so the table is made again.
ALTER TABLE login_sessions RENAME TO login_sessions_v1205;
CREATE TABLE login_sessions (
    id           TEXT PRIMARY KEY,
    kind         TEXT NOT NULL CHECK (kind IN ('claude', 'codex', 'copilot', 'pi', 'grok')),
    provider     TEXT,
    state        TEXT NOT NULL
                 CHECK (state IN ('starting', 'waiting', 'verifying', 'done', 'failed', 'cancelled', 'expired')),
    url          TEXT,
    device_code  TEXT,
    subscription TEXT,
    started_at   TEXT NOT NULL,
    finished_at  TEXT,
    error        TEXT
);
INSERT INTO login_sessions (id, kind, provider, state, url, device_code, subscription, started_at, finished_at, error)
    SELECT id, kind, provider, state, url, device_code, subscription, started_at, finished_at, error FROM login_sessions_v1205;
DROP TABLE login_sessions_v1205;
