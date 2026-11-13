-- Sort after ter-981-dashboard-page.sql on fresh databases.
-- Schema only. Defaults are explicitly initialized by the activation owner.
CREATE TABLE project_page_layouts (
    project TEXT PRIMARY KEY REFERENCES projects(name) ON DELETE CASCADE,
    revision INTEGER NOT NULL DEFAULT 0 CHECK(revision >= 0),
    initialized INTEGER NOT NULL DEFAULT 0 CHECK(initialized IN (0,1))
);
ALTER TABLE project_pages ADD COLUMN builtin TEXT CHECK(builtin IS NULL OR builtin IN (
    'board','tests','schedules','rules','agents','repos','knowledge','pipelines','calendar','logs'
));
CREATE UNIQUE INDEX project_pages_builtin ON project_pages(project,builtin)
    WHERE builtin IS NOT NULL;
