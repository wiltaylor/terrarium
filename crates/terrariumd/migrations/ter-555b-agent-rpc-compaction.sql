-- TER-286: keep admission keys forever, but bound retained response payloads.
ALTER TABLE agent_rpc_answers ADD COLUMN compacted INTEGER NOT NULL DEFAULT 0;
CREATE INDEX agent_rpc_answers_retained ON agent_rpc_answers(created_at)
    WHERE compacted = 0;
