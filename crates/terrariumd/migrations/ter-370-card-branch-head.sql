-- Last observed remote head and the working-time baseline for no-progress.
CREATE TABLE card_branch_heads (
    card TEXT PRIMARY KEY REFERENCES cards(id) ON DELETE CASCADE,
    repo TEXT NOT NULL,
    branch TEXT NOT NULL,
    head TEXT,
    working_seconds INTEGER NOT NULL,
    observed_at TEXT NOT NULL
);
CREATE INDEX cards_branch_watch ON cards(archived, column_id, id)
    WHERE repo IS NOT NULL AND branch IS NOT NULL;
