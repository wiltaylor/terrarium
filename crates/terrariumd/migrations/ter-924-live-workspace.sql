-- Inactive B1 storage only. No route or production caller writes these tables.
-- B4 must use BEGIN IMMEDIATE and synchronous=FULL before dispatch, and gate
-- every workspace use. Identity/seal rows intentionally have no cascading FK
-- to agents or targets: deleting either must not erase retained-reference history.
CREATE TABLE live_workspaces (
    workspace_id TEXT PRIMARY KEY NOT NULL,
    host_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    project_hint TEXT NOT NULL,
    agent_hint TEXT NOT NULL,
    source_dev TEXT NOT NULL,
    source_ino TEXT NOT NULL,
    history TEXT NOT NULL CHECK(history IN ('fresh', 'unknown')),
    UNIQUE(host_id, agent_id),
    UNIQUE(host_id, source_dev, source_ino)
);
CREATE TRIGGER live_workspace_no_update BEFORE UPDATE ON live_workspaces
BEGIN SELECT RAISE(ABORT, 'enrollment is immutable'); END;
CREATE TRIGGER live_workspace_no_delete BEFORE DELETE ON live_workspaces
BEGIN SELECT RAISE(ABORT, 'enrollment is permanent'); END;

CREATE TABLE live_workspace_targets (
    registration_id TEXT PRIMARY KEY NOT NULL,
    host_id TEXT NOT NULL,
    target_id TEXT NOT NULL,
    trust_domain_id TEXT NOT NULL,
    runtime_id TEXT NOT NULL,
    container_id TEXT NOT NULL CHECK(length(container_id) = 64),
    namespace_dev TEXT NOT NULL,
    namespace_ino TEXT NOT NULL,
    -- Cannot represent an enabled target until the B4 activation review.
    activation TEXT NOT NULL DEFAULT 'disabled' CHECK(activation = 'disabled'),
    state TEXT NOT NULL DEFAULT 'quarantined'
        CHECK(state IN ('quarantined', 'reconcile_pending', 'verified', 'destroyed')),
    last_contiguous_revision INTEGER NOT NULL DEFAULT 0 CHECK(last_contiguous_revision >= 0),
    allocated_revision INTEGER NOT NULL DEFAULT 0 CHECK(allocated_revision >= last_contiguous_revision),
    UNIQUE(registration_id, trust_domain_id)
);
CREATE UNIQUE INDEX live_workspace_current_target ON live_workspace_targets(host_id, target_id)
    WHERE state != 'destroyed';
CREATE TRIGGER live_workspace_target_identity BEFORE UPDATE OF
    registration_id, host_id, target_id, trust_domain_id, runtime_id, container_id,
    namespace_dev, namespace_ino ON live_workspace_targets
BEGIN SELECT RAISE(ABORT, 'registration identity is immutable'); END;

CREATE TABLE workspace_reservations (
    workspace_id TEXT PRIMARY KEY NOT NULL REFERENCES live_workspaces(workspace_id),
    trust_domain_id TEXT NOT NULL,
    current_registration_id TEXT,
    owning_sandbox_incarnation TEXT,
    FOREIGN KEY(current_registration_id, trust_domain_id)
        REFERENCES live_workspace_targets(registration_id, trust_domain_id)
);
CREATE INDEX workspace_reservations_registration ON workspace_reservations(current_registration_id)
    WHERE current_registration_id IS NOT NULL;
CREATE TRIGGER workspace_reservation_seal BEFORE UPDATE OF workspace_id, trust_domain_id ON workspace_reservations
BEGIN SELECT RAISE(ABORT, 'workspace domain is permanently sealed'); END;
CREATE TRIGGER workspace_reservation_no_delete BEFORE DELETE ON workspace_reservations
BEGIN SELECT RAISE(ABORT, 'workspace domain is permanently sealed'); END;
CREATE TRIGGER workspace_reservation_occupied BEFORE UPDATE OF current_registration_id ON workspace_reservations
WHEN OLD.current_registration_id IS NOT NULL AND NEW.current_registration_id IS NOT NULL
    AND OLD.current_registration_id != NEW.current_registration_id
BEGIN SELECT RAISE(ABORT, 'workspace registration slot occupied'); END;
CREATE TRIGGER workspace_reservation_fresh BEFORE INSERT ON workspace_reservations
WHEN NOT EXISTS(SELECT 1 FROM live_workspaces WHERE workspace_id = NEW.workspace_id AND history = 'fresh')
BEGIN SELECT RAISE(ABORT, 'unknown workspace history'); END;

CREATE TABLE live_workspace_attachments (
    attachment_id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL REFERENCES workspace_reservations(workspace_id),
    registration_id TEXT NOT NULL REFERENCES live_workspace_targets(registration_id),
    agent_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('attached_verified', 'authority_ended', 'revoke_pending', 'path_revoked_verified', 'quarantined')),
    revoke_revision INTEGER CHECK(revoke_revision > 0)
);
CREATE INDEX live_workspace_attachments_registration ON live_workspace_attachments(registration_id, state, attachment_id);
CREATE INDEX live_workspace_attachments_workspace ON live_workspace_attachments(workspace_id, attachment_id);
CREATE TRIGGER live_workspace_attachment_tombstone BEFORE UPDATE OF revoke_revision ON live_workspace_attachments
WHEN OLD.revoke_revision IS NOT NULL AND NEW.revoke_revision IS NOT OLD.revoke_revision
BEGIN SELECT RAISE(ABORT, 'revoke tombstone is permanent'); END;
CREATE TRIGGER live_workspace_attachment_no_delete BEFORE DELETE ON live_workspace_attachments
BEGIN SELECT RAISE(ABORT, 'attachment history is permanent'); END;
CREATE TRIGGER live_workspace_attachment_no_resurrect BEFORE UPDATE OF state ON live_workspace_attachments
WHEN OLD.revoke_revision IS NOT NULL AND NEW.state = 'attached_verified'
BEGIN SELECT RAISE(ABORT, 'attachment has been revoked'); END;

CREATE TABLE live_workspace_operations (
    operation_id TEXT PRIMARY KEY NOT NULL,
    registration_id TEXT NOT NULL REFERENCES live_workspace_targets(registration_id),
    revision INTEGER NOT NULL CHECK(revision > 0),
    kind TEXT NOT NULL CHECK(kind IN ('register', 'grant', 'revoke', 'start', 'restart', 'recreate', 'destroy', 'reconcile')),
    -- Store the canonical ORIGINAL bytes/hash, never a rewritten superseded body.
    original_request BLOB NOT NULL CHECK(length(original_request) <= 65536),
    original_hash BLOB NOT NULL CHECK(length(original_hash) = 32),
    delivery TEXT NOT NULL CHECK(delivery IN ('apply', 'superseded')),
    phase TEXT NOT NULL CHECK(phase IN ('intent', 'helper_committed', 'daemon_committed', 'quarantined')),
    result BLOB,
    CHECK(delivery != 'superseded' OR kind = 'grant'),
    CHECK(phase NOT IN ('helper_committed', 'daemon_committed') OR result IS NOT NULL),
    UNIQUE(registration_id, revision)
);
CREATE INDEX live_workspace_operations_pending ON live_workspace_operations(registration_id, phase, revision);
CREATE TRIGGER live_workspace_operation_identity BEFORE UPDATE OF
    operation_id, registration_id, revision, kind, original_request, original_hash ON live_workspace_operations
BEGIN SELECT RAISE(ABORT, 'original operation is immutable'); END;
CREATE TRIGGER live_workspace_operation_delivery BEFORE UPDATE OF delivery ON live_workspace_operations
WHEN OLD.phase != 'intent' OR OLD.delivery != 'apply' OR NEW.delivery != 'superseded'
BEGIN SELECT RAISE(ABORT, 'delivery already decided'); END;
CREATE TRIGGER live_workspace_operation_no_delete BEFORE DELETE ON live_workspace_operations
BEGIN SELECT RAISE(ABORT, 'operation history is permanent'); END;
