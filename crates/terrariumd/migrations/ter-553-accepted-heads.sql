-- The reviewed commit of each card in the merge queue (TER-553). A move
-- into the merge queue opens the row with no commit; the daemon then reads
-- the head of the card branch on its remote and records it. A move out of
-- the merge queue removes the row. A merge job takes the recorded commit.
CREATE TABLE card_accepted_heads (
    card TEXT PRIMARY KEY REFERENCES cards(id) ON DELETE CASCADE,
    -- A new ULID for each move into the queue: a late read of an earlier
    -- round does not fill a later one.
    round TEXT NOT NULL,
    accepted_at TEXT NOT NULL,
    branch TEXT,
    commit_sha TEXT,
    recorded_at TEXT,
    -- Why the head could not be read yet (no branch on the remote), and
    -- when the read last failed: the next read waits a while.
    note TEXT,
    tried_at TEXT
);
-- The cards that are in the merge queue at the upgrade get a round too, so
-- the queue goes on: the daemon records their head at its first read.
INSERT INTO card_accepted_heads (card, round, accepted_at)
SELECT c.id, 'upgrade-' || c.id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
FROM cards c JOIN board_columns b ON b.id = c.column_id
WHERE c.archived = 0
  AND (b.role = 'merge_queue' OR (b.role IS NULL AND b.name = 'merge_queue'));
