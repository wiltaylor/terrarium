-- No agent FK: failed cleanup must survive agent removal and daemon restart.
CREATE TABLE agent_target_roots (
    agent TEXT NOT NULL,
    project TEXT NOT NULL,
    name TEXT NOT NULL,
    target TEXT NOT NULL,
    definition TEXT NOT NULL,
    PRIMARY KEY (agent, target)
);
CREATE TABLE agent_target_root_warnings (
    agent TEXT NOT NULL,
    target TEXT NOT NULL,
    PRIMARY KEY (agent, target)
);
