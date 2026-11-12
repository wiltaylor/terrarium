CREATE TABLE project_git_settings (
    project TEXT PRIMARY KEY REFERENCES projects(name) ON DELETE CASCADE,
    settings TEXT NOT NULL
);
CREATE TABLE repo_git_settings (
    repo TEXT PRIMARY KEY REFERENCES repos(id) ON DELETE CASCADE,
    settings TEXT NOT NULL
);
