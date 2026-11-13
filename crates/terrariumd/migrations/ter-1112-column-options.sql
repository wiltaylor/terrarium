-- TER-1112 (kanban rework C1a): the fixed move checks become options of a
-- column, with today's behaviour as the backfill, and each column gets the
-- settings of its future queue (C3 implements the queue; this stores them).
-- `options_version` is the optimistic concurrency token of
-- `PUT /api/projects/{name}/board/columns/{column}/options`.
ALTER TABLE board_columns ADD COLUMN require_comment_on_enter INTEGER NOT NULL DEFAULT 0
  CHECK(require_comment_on_enter IN (0, 1));
ALTER TABLE board_columns ADD COLUMN require_dependencies_met INTEGER NOT NULL DEFAULT 0
  CHECK(require_dependencies_met IN (0, 1));
ALTER TABLE board_columns ADD COLUMN terminal_for_members INTEGER NOT NULL DEFAULT 0
  CHECK(terminal_for_members IN (0, 1));
ALTER TABLE board_columns ADD COLUMN clear_eta_on_enter INTEGER NOT NULL DEFAULT 1
  CHECK(clear_eta_on_enter IN (0, 1));
ALTER TABLE board_columns ADD COLUMN queue_release TEXT NOT NULL DEFAULT 'wip'
  CHECK(queue_release IN ('wip', 'batch', 'job'));
ALTER TABLE board_columns ADD COLUMN queue_batch_size INTEGER
  CHECK(queue_batch_size IS NULL OR queue_batch_size > 0);
ALTER TABLE board_columns ADD COLUMN queue_max_wait_seconds INTEGER
  CHECK(queue_max_wait_seconds IS NULL OR queue_max_wait_seconds > 0);
ALTER TABLE board_columns ADD COLUMN queue_max_batch_cards INTEGER
  CHECK(queue_max_batch_cards IS NULL OR queue_max_batch_cards > 0);
ALTER TABLE board_columns ADD COLUMN options_version INTEGER NOT NULL DEFAULT 1;

-- Today's checks: a move into the `working` column needs met dependencies,
-- a move into the `blocked` column needs a comment, and `done` is final for
-- members. Every column clears the ETA of a card that enters it.
UPDATE board_columns SET require_dependencies_met = 1 WHERE role = 'working';
UPDATE board_columns SET require_comment_on_enter = 1 WHERE role = 'blocked';
UPDATE board_columns SET terminal_for_members = 1 WHERE name = 'done';
