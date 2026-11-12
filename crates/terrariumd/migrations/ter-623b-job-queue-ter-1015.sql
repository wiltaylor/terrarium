-- TER-1015: sort after ter-623, which rebuilds jobs and its indexes.
CREATE INDEX jobs_state_created ON jobs(state, created_at);
CREATE INDEX jobs_state_started ON jobs(state, started_at);
