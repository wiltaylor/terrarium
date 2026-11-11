-- The host reconnect journal. Payloads include bearer-like resume tokens
-- and command output, so they use the same encryption as secret rows.
CREATE TABLE host_resume (
    id TEXT PRIMARY KEY NOT NULL,
    sealed BLOB NOT NULL
);
