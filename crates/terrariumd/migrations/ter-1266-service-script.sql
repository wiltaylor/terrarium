-- Services C3 (TER-1266, `crate::service_script`): the run of a script
-- service request on its target.
--
-- `script_dir`: the run's directory on the target. `''` while the daemon
-- makes it; set when the run has started. A `running` script request with
-- a `script_dir` and no live run (the daemon restarted) ends as `failed`:
-- a run is never started twice (Q4 A, no retry).
-- `response_size`: the bytes of the response files under
-- `services/payloads/<id>/response/`. They count toward the per-service
-- and the global payload caps with `payload_size`, until
-- `payload_deleted_at` is set.
ALTER TABLE service_requests ADD COLUMN script_dir TEXT;
ALTER TABLE service_requests ADD COLUMN response_size INTEGER NOT NULL DEFAULT 0;
