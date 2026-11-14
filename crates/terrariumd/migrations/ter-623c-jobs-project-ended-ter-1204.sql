-- The ended jobs of a project by end time (TER-1204): the stale test of a
-- `batch` column queue (`store/board/queues.rs`, `stale_in_batch`).
CREATE INDEX IF NOT EXISTS jobs_project_ended ON jobs(project, ended_at);
