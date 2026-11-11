CREATE INDEX threads_request_close ON threads(request, status, updated_at, id)
WHERE request = 1 AND status = 'waiting_operator' AND close_proposal IS NOT NULL;
