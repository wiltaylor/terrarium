-- TER-1114 (kanban rework C3): the queues of the board columns. Only a
-- project with `board_flow = columns` writes rows here. A move into a
-- column that has no free slot is accepted: the card is in the column
-- (`cards.column_id`) and waits in its queue (`admitted_at IS NULL`). When
-- it gets a slot, the daemon admits it and the `lands` rules run.
--
-- A card admitted by the release `job` or `batch` keeps its row with
-- `admitted_at` set: it holds the job slot or belongs to the open batch
-- until it leaves the column (or, for `job`, until a job of the card
-- ends). A card admitted by the release `wip` has no row.
--
-- `seq` gives the queue order (first in, first out). `held_eta` is an ETA
-- that was set while the card waited: it is set when the card is admitted
-- (a duration counts from then), as set by `held_eta_by`.
CREATE TABLE card_queue (
  card        TEXT PRIMARY KEY REFERENCES cards(id) ON DELETE CASCADE,
  column_id   INTEGER NOT NULL REFERENCES board_columns(id) ON DELETE CASCADE,
  seq         INTEGER NOT NULL,
  queued_at   TEXT NOT NULL,
  admitted_at TEXT,
  held_eta    TEXT,
  held_eta_by TEXT
);
CREATE INDEX card_queue_order ON card_queue(column_id, seq);
