-- Services (TER-1285, `crate::store::services::prune_ended`): the sweep
-- now prunes `service_requests` rows. The comment in
-- `ter-1264-services.sql` ("Nothing prunes the rows yet") is out of date:
-- an ended request whose payload is gone is deleted once it ended more
-- than `TERRARIUM_SERVICE_REQUEST_KEEP` ago (default 30 days).
--
-- The prune walks this index in `ended_at` order and stops after its
-- batch, so a tick with nothing due reads no ended rows. Only rows with
-- their payload gone are in it.
CREATE INDEX service_requests_prunable ON service_requests(ended_at)
    WHERE payload_deleted_at IS NOT NULL;
