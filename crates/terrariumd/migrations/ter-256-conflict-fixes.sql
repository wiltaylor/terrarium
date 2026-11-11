CREATE TABLE conflict_fixes (
    -- Keep history across the later jobs-table rebuilds.
    job TEXT NOT NULL,
    card TEXT NOT NULL REFERENCES cards(id),
    project TEXT NOT NULL REFERENCES projects(name),
    template TEXT NOT NULL,
    task TEXT NOT NULL,
    done INTEGER NOT NULL DEFAULT 0,
    retry_at INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    PRIMARY KEY (job, card)
);
CREATE INDEX conflict_fixes_pending ON conflict_fixes(done, retry_at);
CREATE INDEX conflict_fixes_card_pending ON conflict_fixes(card, done);
-- The ready-card query orders equal positions by the card's TEXT id,
-- not rowid. Cover that tie-breaker without a temporary sort.
CREATE INDEX cards_dispatch_order ON cards(column_id, position, id);
