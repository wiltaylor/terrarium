-- The time of the admission (unix seconds). The prune timer deletes answers
-- past `agent_rpc::ANSWER_MAX_AGE` in bounded batches on this index. Rows
-- from before this migration count from the upgrade, so a retry across the
-- upgrade still replays.
ALTER TABLE agent_rpc_answers ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0;
UPDATE agent_rpc_answers SET created_at = CAST(strftime('%s', 'now') AS INTEGER);
CREATE INDEX agent_rpc_answers_created ON agent_rpc_answers(created_at);
