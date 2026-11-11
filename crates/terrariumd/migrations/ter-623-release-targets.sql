-- TER-623 (review-ter-623-r3 finding 2): the release targets that the
-- protection of a release target covers, kept apart from the delivery
-- config. Each set of a project's `release_target` adds the target and its
-- machine (`store::jobs::machine_key`); a switch or a clear removes
-- nothing. Only the operator retires an entry (`DELETE
-- /api/release-targets/{name}`). The rows of the configs set so far come
-- in with no machine: the check also reads the target's machine now.
CREATE TABLE release_targets (
    target   TEXT NOT NULL,
    machine  TEXT NOT NULL DEFAULT '',
    project  TEXT NOT NULL,
    added_by TEXT NOT NULL,
    added_at TEXT NOT NULL,
    PRIMARY KEY (target, machine)
);
INSERT OR IGNORE INTO release_targets (target, machine, project, added_by, added_at)
    SELECT json_extract(def, '$.release_target'), '', project, updated_by, updated_at
    FROM project_delivery
    WHERE coalesce(json_extract(def, '$.release_target'), '') != '';
