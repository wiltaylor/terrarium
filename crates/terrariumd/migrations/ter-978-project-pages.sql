CREATE TABLE project_pages (
    project TEXT NOT NULL REFERENCES projects(name) ON DELETE CASCADE,
    slug TEXT NOT NULL,
    title TEXT NOT NULL,
    position INTEGER NOT NULL CHECK(position >= 0),
    widgets TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    updated_by TEXT NOT NULL,
    PRIMARY KEY(project, slug)
);
CREATE INDEX project_pages_order ON project_pages(project, position, slug);
