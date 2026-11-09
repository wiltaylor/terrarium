-- Durable once-per-card trouble notices and project thresholds.
CREATE TABLE card_trouble (
    card TEXT NOT NULL REFERENCES cards(id) ON DELETE CASCADE,
    signal TEXT NOT NULL,
    active INTEGER NOT NULL,
    first_at TEXT NOT NULL,
    PRIMARY KEY(card, signal)
);
CREATE TABLE project_trouble (
    project TEXT PRIMARY KEY REFERENCES projects(name) ON DELETE CASCADE,
    settings TEXT NOT NULL CHECK(json_valid(settings))
);
