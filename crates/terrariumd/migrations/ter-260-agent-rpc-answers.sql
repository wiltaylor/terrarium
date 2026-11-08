-- A NULL answer records a started write. Never replay it after a crash.
CREATE TABLE agent_rpc_answers (
    agent TEXT NOT NULL,
    request_id TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    answer TEXT,
    PRIMARY KEY (agent, request_id)
);
