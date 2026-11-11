-- Default Inbox pages and their badge share this actionable-row predicate.
-- Cover the page keys and counts without reading bodies of excluded notices.
CREATE INDEX questions_default_inbox ON questions(project, priority, created_at, kind, id)
WHERE status = 'open' AND thread IS NULL
  AND (kind IN ('question', 'bundle', 'tool_run') OR (kind = 'notice' AND priority = 'critical'));
