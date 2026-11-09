-- Both post-entry history and the single status immediately before entry
-- must seek by assignee and sequence, never walk an assignee's lifetime.
CREATE INDEX events_trouble_agent ON events(agent_id, kind, seq)
    WHERE kind IN ('status', 'wait', 'agent.reset');
