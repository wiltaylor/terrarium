-- Last observed remote head and the working-time baseline for no-progress.
-- `head` is NULL while the branch is missing on the remote. `observed_at` is
-- when the current head was first seen, `checked_at` the last successful read.
CREATE TABLE card_branch_heads (
    card TEXT PRIMARY KEY REFERENCES cards(id) ON DELETE CASCADE,
    repo TEXT NOT NULL,
    branch TEXT NOT NULL,
    head TEXT,
    working_seconds INTEGER NOT NULL,
    observed_at TEXT NOT NULL,
    checked_at TEXT NOT NULL
);
-- The remote scan walks the working and research columns, then each column's
-- repo cards in id order from a rotating cursor.
CREATE INDEX board_columns_role_watch ON board_columns(role);
CREATE INDEX board_columns_name_watch ON board_columns(lower(name));
CREATE INDEX cards_branch_watch ON cards(column_id, id)
    WHERE archived = 0 AND repo IS NOT NULL;
