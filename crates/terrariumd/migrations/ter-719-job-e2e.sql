-- Preserve historical jobs while extending the job-kind CHECK constraint.
ALTER TABLE jobs ADD COLUMN kind_e2e TEXT NOT NULL DEFAULT 'validate'
    CHECK (kind_e2e IN ('validate', 'merge', 'release', 'e2e'));
UPDATE jobs SET kind_e2e = kind;
ALTER TABLE jobs DROP COLUMN kind;
ALTER TABLE jobs RENAME COLUMN kind_e2e TO kind;
