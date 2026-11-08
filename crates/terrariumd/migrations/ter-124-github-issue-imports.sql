CREATE TABLE github_issue_imports (
    project TEXT NOT NULL REFERENCES projects(name) ON DELETE CASCADE,
    url TEXT NOT NULL,
    card TEXT NOT NULL REFERENCES cards(id),
    PRIMARY KEY(project, url)
);
