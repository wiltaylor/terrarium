-- Admission history is permanent. Ending an exec or deleting an agent cannot
-- prove that its processes, directory FDs or sockets no longer exist.
CREATE TABLE live_workspace_use_history (
    use_id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL REFERENCES live_workspaces(workspace_id),
    kind TEXT NOT NULL CHECK(kind IN ('owner_sandbox', 'live', 'other')),
    scope TEXT NOT NULL,
    created_at TEXT NOT NULL
) WITHOUT ROWID;
CREATE INDEX live_workspace_use_history_workspace ON live_workspace_use_history(workspace_id, kind, scope);
CREATE TRIGGER live_workspace_use_history_no_update BEFORE UPDATE ON live_workspace_use_history
BEGIN SELECT RAISE(ABORT, 'workspace use history is immutable'); END;
CREATE TRIGGER live_workspace_use_history_no_delete BEFORE DELETE ON live_workspace_use_history
BEGIN SELECT RAISE(ABORT, 'workspace use history is permanent'); END;
CREATE TRIGGER live_workspace_use_history_no_replace BEFORE INSERT ON live_workspace_use_history
WHEN EXISTS(SELECT 1 FROM live_workspace_use_history WHERE use_id = NEW.use_id)
BEGIN SELECT RAISE(ABORT, 'workspace use history already exists'); END;
