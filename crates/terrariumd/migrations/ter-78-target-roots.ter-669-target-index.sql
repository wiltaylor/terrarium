-- Definition edits invalidate one target's root plans, never scan other targets.
CREATE INDEX agent_target_roots_target ON agent_target_roots(target);
CREATE INDEX agent_target_root_warnings_target ON agent_target_root_warnings(target);
