-- TER-1284: a card that a `batch` column put back in its queue after a
-- batch round in which it got no job is stale (`stale_at`, the time of
-- the return). It does not count for a batch and is not admitted again
-- until something changes: a new card in the queue, a job end of a card
-- of the column, an edit of the card (`cards.updated_at` after
-- `stale_at`), its reviewed commit, or a change of the board columns.
-- So a column whose `lands` rules queue no job does not admit the same
-- cards again every 120 s.
ALTER TABLE card_queue ADD COLUMN stale_at TEXT;
