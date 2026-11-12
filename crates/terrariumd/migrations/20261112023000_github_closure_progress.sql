CREATE TABLE github_closure_cursor (
    project TEXT NOT NULL REFERENCES projects(name) ON DELETE CASCADE,
    repo TEXT NOT NULL,
    after_issue INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(project, repo)
);
CREATE TABLE github_closed_issues (
    project TEXT NOT NULL REFERENCES projects(name) ON DELETE CASCADE,
    repo TEXT NOT NULL,
    issue INTEGER NOT NULL,
    PRIMARY KEY(project, repo, issue)
);
