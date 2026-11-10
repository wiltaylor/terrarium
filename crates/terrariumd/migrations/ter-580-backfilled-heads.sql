-- TER-580: a round that the TER-553 upgrade backfilled was not opened by a
-- review accept, so the tip of its branch at the upgrade (or at the first
-- read after it) is not a reviewed commit. Mark those rounds: the
-- background read never fills them, and a merge takes the card only after
-- a fresh accept (a move out of the merge queue and back in).
ALTER TABLE card_accepted_heads ADD COLUMN needs_accept INTEGER NOT NULL DEFAULT 0;
UPDATE card_accepted_heads
SET needs_accept = 1, branch = NULL, commit_sha = NULL, recorded_at = NULL,
    note = 'the round was opened by the upgrade, not by a review accept: accept the card again',
    tried_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
WHERE round LIKE 'upgrade-%';
