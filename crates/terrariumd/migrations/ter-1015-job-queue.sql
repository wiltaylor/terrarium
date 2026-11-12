CREATE INDEX jobs_state_created ON jobs(state, created_at);
CREATE INDEX jobs_state_started ON jobs(state, started_at);
-- The supervisor status write looks up waits by waiter and by target.
CREATE INDEX waits_agent_state_created ON waits(agent, state, created_at, id);
CREATE INDEX waits_target_state_created ON waits(target_agent, state, kind, created_at, id);
