-- Named to sort after ter-78-target-roots.sql, which makes the table.
-- The base of an owned root when it is not the base of the definition: the
-- home fallback of a target without work_root (TER-498). NULL: the definition.
ALTER TABLE agent_target_roots ADD COLUMN base TEXT;
