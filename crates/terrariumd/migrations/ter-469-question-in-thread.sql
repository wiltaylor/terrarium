-- GitHub #30 (TER-469): the operator moves an open question or notice into
-- an Inbox thread with its agent. The question gets the status `in_thread`:
-- out of the open count, and answered, acknowledged, or retracted from the
-- thread. The status column is made again with the new CHECK, as legacy
-- migration 30 did; its indexes go first. `threads.from_question` is the
-- question that the thread was opened from.
DROP INDEX questions_agent;
DROP INDEX questions_status;
ALTER TABLE questions ADD COLUMN status_v469 TEXT NOT NULL DEFAULT 'open'
    CHECK (status_v469 IN ('open', 'in_thread', 'answered', 'acknowledged', 'retracted'));
UPDATE questions SET status_v469 = status;
ALTER TABLE questions DROP COLUMN status;
ALTER TABLE questions RENAME COLUMN status_v469 TO status;
CREATE INDEX questions_agent ON questions(agent, status);
CREATE INDEX questions_status ON questions(status, project);
ALTER TABLE threads ADD COLUMN from_question TEXT;
