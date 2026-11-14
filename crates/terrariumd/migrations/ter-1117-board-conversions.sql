-- TER-1117 (kanban rework C5): the conversion of each board to column
-- rules at the upgrade. One row for each converted project, with what the
-- board had before, so that one project can be reverted
-- (`board_convert::revert`): the flow and its time, the dispatcher
-- settings (not changed by the conversion), the hash of `rules.wcl`, the
-- IDs of the column rules written (JSON list), the queue of the batch
-- column before (JSON `{column, queue}`), and the report (JSON).
CREATE TABLE board_conversions (
  project           TEXT PRIMARY KEY,
  converted_at      TEXT NOT NULL,
  previous_flow     TEXT NOT NULL,
  previous_flow_at  TEXT,
  card_dispatcher   TEXT,
  review_dispatcher TEXT,
  rules_hash        TEXT,
  rule_ids          TEXT NOT NULL,
  queue_before      TEXT,
  report            TEXT NOT NULL
);
