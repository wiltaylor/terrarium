-- TER-1113 (kanban rework C2a): the run log of the column rules. A column
-- rule has a stable ID (board_column_rules.id), and two columns may have
-- rules of the same name, so a run records the rule's ID and its column.
-- `rule` holds `<column>/<name>` for these runs, which keeps
-- UNIQUE (project, rule, event_seq, lane) one row per run. The rate limits
-- of a column rule count its runs by `rule_id`, so a rename of the rule or
-- of its column does not reset them. Legacy runs (the pack, rules.wcl)
-- have NULL in both.
ALTER TABLE rule_runs ADD COLUMN rule_id TEXT;
ALTER TABLE rule_runs ADD COLUMN column_name TEXT;
CREATE INDEX rule_runs_rule_id ON rule_runs(rule_id, created_at) WHERE rule_id IS NOT NULL;
