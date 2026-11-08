ALTER TABLE threads ADD COLUMN monitor TEXT;
ALTER TABLE threads ADD COLUMN monitor_done INTEGER NOT NULL DEFAULT 0;
ALTER TABLE thread_pending_wakes RENAME TO thread_pending_wakes_old;
CREATE TABLE thread_pending_wakes (
    thread TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    reason TEXT NOT NULL CHECK(reason IN ('blocked', 'review', 'monitor_green', 'monitor_red')),
    PRIMARY KEY(thread, reason)
);
INSERT INTO thread_pending_wakes SELECT * FROM thread_pending_wakes_old;
DROP TABLE thread_pending_wakes_old;
