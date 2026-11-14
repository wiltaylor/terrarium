-- TER-1324: the default Inbox and its badge list every open notice, not only
-- the critical ones (TER-688 hid the normal and urgent notices). Keep this
-- predicate aligned with DEFAULT_QUESTION_WHERE in src/store/inbox.rs.
DROP INDEX questions_default_inbox;
CREATE INDEX questions_default_inbox ON questions(project, priority, created_at, kind, id)
WHERE status = 'open' AND thread IS NULL
  AND kind IN ('question', 'bundle', 'tool_run', 'notice');
