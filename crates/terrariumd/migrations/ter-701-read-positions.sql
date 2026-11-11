CREATE TABLE read_positions (
    owner TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('thread', 'mail', 'card')),
    item TEXT NOT NULL,
    read_at TEXT NOT NULL,
    PRIMARY KEY (owner, kind, item)
) WITHOUT ROWID;
