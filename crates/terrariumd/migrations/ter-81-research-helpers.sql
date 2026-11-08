-- Helper history survives removal of agents and spawn rows.
CREATE TABLE research_helpers (
    id TEXT PRIMARY KEY,
    card TEXT NOT NULL,
    project TEXT NOT NULL,
    spawned_at TEXT NOT NULL,
    finished_at TEXT
);
CREATE INDEX research_helpers_project ON research_helpers(project, spawned_at);
CREATE TABLE research_helper_alerts (card TEXT PRIMARY KEY);
