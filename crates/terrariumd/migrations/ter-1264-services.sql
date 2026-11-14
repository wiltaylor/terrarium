-- Services C1 (TER-1264, `crate::store::services`): the service
-- definitions and their request queue.
--
-- `services` keeps each definition as JSON (`ServiceDef`) from a
-- `services/<name>.toml` file (source `file`) or the API (source `api`),
-- as `tools` does.
--
-- `removed_at`: a file service whose file went (or no longer checks) while
-- it had open requests. It stays so that those requests can end, and it
-- takes no new call; the next reload with no open request removes it.
--
-- `service_requests` is the queue and its history. `service` is not a
-- foreign key: the rows of a removed service stay. Nothing prunes the rows
-- yet; the retention sweep deletes only the payload files. The payload
-- bytes are files in the daemon's state directory
-- (`services/payloads/<id>/request`); `payload_size` counts toward the
-- per-service and the global cap until `payload_deleted_at` is set.
CREATE TABLE services (
    name       TEXT PRIMARY KEY,
    project    TEXT,                              -- NULL: a global service
    def        TEXT NOT NULL,                     -- JSON ServiceDef
    source     TEXT NOT NULL DEFAULT 'api' CHECK (source IN ('file', 'api')),
    removed_at TEXT,                              -- NULL: active
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX services_project ON services(project);

CREATE TABLE service_requests (
    id                 TEXT PRIMARY KEY,          -- ULID, so the order is the arrival order
    service            TEXT NOT NULL,
    project            TEXT,                      -- the service's project when called
    caller             TEXT NOT NULL,             -- actor label: operator or agent:<project>/<name>
    caller_agent       TEXT,                      -- the agent ID, NULL for the operator
    state              TEXT NOT NULL DEFAULT 'queued' CHECK (state IN
                         ('queued', 'waiting', 'running', 'done', 'failed', 'cancelled', 'timed_out')),
    request            TEXT NOT NULL,             -- the request JSON
    payload_size       INTEGER NOT NULL DEFAULT 0,
    payload_sha256     TEXT,                      -- NULL: no payload
    response           TEXT,                      -- the response JSON (C3-C5)
    exit_code          INTEGER,
    error              TEXT,
    wait_until         TEXT NOT NULL,             -- the wait limit: timed out when not started by then
    created_at         TEXT NOT NULL,
    started_at         TEXT,
    ended_at           TEXT,
    payload_deleted_at TEXT
);
CREATE INDEX service_requests_queue ON service_requests(service, state, id);
CREATE INDEX service_requests_caller ON service_requests(caller_agent, id);
CREATE INDEX service_requests_state ON service_requests(state, wait_until);
