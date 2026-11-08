-- Opt-in ready-card dispatcher. NULL keeps existing projects disabled.
ALTER TABLE projects ADD COLUMN card_dispatcher TEXT;

CREATE TABLE card_dispatch_retries (
    card TEXT PRIMARY KEY REFERENCES cards(id) ON DELETE CASCADE,
    failures INTEGER NOT NULL,
    retry_at INTEGER NOT NULL
);
