CREATE TABLE conflict_fixes (
    job TEXT NOT NULL REFERENCES jobs(id),
    card TEXT NOT NULL REFERENCES cards(id),
    project TEXT NOT NULL REFERENCES projects(name),
    template TEXT NOT NULL,
    owner TEXT,
    task TEXT NOT NULL,
    done INTEGER NOT NULL DEFAULT 0,
    retry_at INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (job, card)
);
CREATE INDEX conflict_fixes_pending ON conflict_fixes(done, retry_at);
