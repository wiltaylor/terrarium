-- Opt-in ready-card dispatcher. NULL keeps existing projects disabled.
ALTER TABLE projects ADD COLUMN card_dispatcher TEXT;
