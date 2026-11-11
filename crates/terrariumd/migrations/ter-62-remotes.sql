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
