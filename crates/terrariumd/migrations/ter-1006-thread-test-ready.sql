ALTER TABLE threads ADD COLUMN test_ready_cards TEXT;
CREATE INDEX threads_test_ready_scan ON threads(id) WHERE tracking_at IS NOT NULL AND status != 'closed';
