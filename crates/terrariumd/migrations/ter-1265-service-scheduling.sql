-- Services C2 (TER-1265, `crate::service_sched`): the scheduling of a
-- service request.
--
-- `reservation_id`: the queued reservation of the request's resource
-- claim (`reservations`), NULL for a service without a claim.
-- `lease_id`: the target lease of the agent that runs the request (C4).
-- `run_until`: the start plus the service's `max_run_time`; a running
-- request past it ends as `timed_out`.
-- `released_at`: the reservation and the lease of an ended request were
-- given back. NULL on an ended request: the scheduler still has to.
-- `retry_at`: a reservation was refused for now (409); the scheduler tries
-- again at this time.
ALTER TABLE service_requests ADD COLUMN reservation_id TEXT;
ALTER TABLE service_requests ADD COLUMN lease_id TEXT;
ALTER TABLE service_requests ADD COLUMN run_until TEXT;
ALTER TABLE service_requests ADD COLUMN released_at TEXT;
ALTER TABLE service_requests ADD COLUMN retry_at TEXT;
CREATE INDEX service_requests_release ON service_requests(released_at, state);
