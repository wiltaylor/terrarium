-- Start-of-day schedules (TER-450, GitHub #29). A schedule keeps the
-- local day of its last start-of-day run (one run per day) and the last
-- detected start (JSON: cause, at). A run gets the trigger 'day_start'
-- and the detected start that planned it. SQLite cannot change a CHECK,
-- so `schedule_runs` is made again, as legacy migration 20 did.
ALTER TABLE schedules ADD COLUMN day_start_done TEXT;
ALTER TABLE schedules ADD COLUMN day_start_last TEXT;
ALTER TABLE schedule_runs RENAME TO schedule_runs_v450;
CREATE TABLE schedule_runs (
    id          TEXT PRIMARY KEY,
    schedule    TEXT NOT NULL REFERENCES schedules(id) ON DELETE CASCADE,
    trigger     TEXT NOT NULL CHECK (trigger IN ('cron', 'manual', 'catch_up', 'event', 'day_start')),
    status      TEXT NOT NULL CHECK (status IN ('running', 'ok', 'failed', 'skipped')),
    started_at  TEXT NOT NULL,
    finished_at TEXT,
    summary     TEXT NOT NULL DEFAULT '',
    output      TEXT NOT NULL DEFAULT '{}',
    error       TEXT,
    actor       TEXT NOT NULL,
    event_seq   INTEGER,
    day_start   TEXT
);
INSERT INTO schedule_runs (id, schedule, trigger, status, started_at, finished_at, summary, output, error, actor, event_seq)
    SELECT id, schedule, trigger, status, started_at, finished_at, summary, output, error, actor, event_seq FROM schedule_runs_v450;
DROP TABLE schedule_runs_v450;
CREATE INDEX schedule_runs_schedule ON schedule_runs(schedule, id);
