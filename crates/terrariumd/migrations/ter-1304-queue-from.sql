-- TER-1304: the column a queued card came from, and its role, as the
-- queuing move had them. The `admitted` event carries them as `from` and
-- `from_role` (with `to` and `to_role`), as the `moved` event does, so a
-- `lands` rule with a `data.from_role` condition also matches when the
-- card is admitted. NULL for a row that no move wrote (a card that landed
-- in a `job` column, or a card admitted under `wip` and put back by a
-- change to `batch`).
ALTER TABLE card_queue ADD COLUMN from_column TEXT;
ALTER TABLE card_queue ADD COLUMN from_role TEXT;
