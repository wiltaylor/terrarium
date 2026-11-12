CREATE TABLE release_attachments (
    job TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    sha256 TEXT NOT NULL
);
CREATE TABLE release_successors (
    job TEXT PRIMARY KEY,
    predecessor TEXT NOT NULL,
    attachment_size INTEGER NOT NULL,
    attachment_sha256 TEXT NOT NULL
);
CREATE INDEX release_successors_predecessor ON release_successors(predecessor);
