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
