CREATE INDEX jobs_state_created ON jobs(state, created_at);
CREATE INDEX jobs_state_started ON jobs(state, started_at);
