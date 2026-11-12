-- Conservative pre-enrollment history. It is not erased with an agent or
-- target. Unknown historic uses cannot later be declared fresh by enrollment.
CREATE TABLE live_workspace_prior_use (
    agent_id TEXT PRIMARY KEY,
    first_used_at TEXT NOT NULL
) WITHOUT ROWID;
CREATE TRIGGER live_workspace_prior_use_no_update
BEFORE UPDATE ON live_workspace_prior_use BEGIN
    SELECT RAISE(ABORT, 'workspace prior use is immutable');
END;
CREATE TRIGGER live_workspace_prior_use_no_delete
BEFORE DELETE ON live_workspace_prior_use BEGIN
    SELECT RAISE(ABORT, 'workspace prior use is permanent');
END;
CREATE TRIGGER live_workspace_prior_use_no_replace
BEFORE INSERT ON live_workspace_prior_use
WHEN EXISTS (SELECT 1 FROM live_workspace_prior_use WHERE agent_id = NEW.agent_id)
BEGIN
    SELECT RAISE(ABORT, 'workspace prior use already recorded');
END;
