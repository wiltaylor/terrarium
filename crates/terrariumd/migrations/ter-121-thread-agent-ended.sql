ALTER TABLE threads ADD COLUMN agent_end_outcome TEXT;
ALTER TABLE threads ADD COLUMN restart_context_sent INTEGER NOT NULL DEFAULT 0;
