-- Cover filtered Inbox candidate pages without reading question bodies.
CREATE INDEX questions_inbox_pages ON questions(
    (status = 'open') DESC,
    (status = 'open' AND priority = 'critical') DESC,
    (status = 'open' AND priority IN ('urgent', 'critical')) DESC,
    created_at DESC, (kind || ':' || id) DESC,
    project, kind, status, id
) WHERE thread IS NULL;
