-- TER-1120 (kanban rework C1b): the structured rules of a board column.
-- Each rule belongs to one column and has a stable ID, so two columns may
-- have rules of the same name. `position` orders the rules of a column.
-- `definition_json` is the rule without its ID and name
-- (`terrarium_core::board_rules::ColumnRule`), checked on each write;
-- `model` is the version of that shape. A write of the rules raises the
-- column's `options_version` (ter-1112-column-options.sql). A removed
-- column takes its rules with it. The engine does not run these rules yet
-- (C2).
CREATE TABLE board_column_rules (
  id TEXT PRIMARY KEY,
  column_id INTEGER NOT NULL REFERENCES board_columns(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  position INTEGER NOT NULL CHECK(position >= 0),
  model INTEGER NOT NULL DEFAULT 1 CHECK(model >= 1),
  definition_json TEXT NOT NULL CHECK(json_valid(definition_json)),
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE(column_id, name)
);
CREATE INDEX board_column_rules_order ON board_column_rules(column_id, position);
