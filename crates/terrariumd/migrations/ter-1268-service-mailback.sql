-- Services C5 (TER-1268, `crate::service_mail`): the result mail and the
-- failure log.
--
-- `mailed_at`: the one result mail to the caller was written (or could
-- not be, or was not needed: the caller cancelled it). An ended request
-- with no `mailed_at` is due a mail; the sweep and the cancel route send it.
-- `log_tail`: the end of the run's log, for a failed run (C3, C4); the
-- failure mail carries it.
--
-- The requests that ended before this migration had their only mail (a
-- time-out) or none: mark them so that no late mail goes out.
ALTER TABLE service_requests ADD COLUMN mailed_at TEXT;
ALTER TABLE service_requests ADD COLUMN log_tail TEXT;
UPDATE service_requests SET mailed_at = COALESCE(ended_at, created_at)
 WHERE state IN ('done', 'failed', 'cancelled', 'timed_out');
CREATE INDEX service_requests_unmailed ON service_requests(state) WHERE mailed_at IS NULL;
