ALTER TABLE group_steps ADD COLUMN started_at TEXT;
ALTER TABLE group_steps ADD COLUMN finished_at TEXT;
CREATE INDEX group_steps_history ON group_steps(name, state, group_card)
    WHERE started_at IS NOT NULL AND finished_at IS NOT NULL;
