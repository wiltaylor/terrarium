-- Opt-in ready-card dispatcher. NULL keeps existing projects disabled.
ALTER TABLE projects ADD COLUMN card_dispatcher TEXT;

CREATE TABLE card_dispatch_retries (
    card TEXT PRIMARY KEY REFERENCES cards(id) ON DELETE CASCADE,
    failures INTEGER NOT NULL,
    retry_at INTEGER NOT NULL
);

CREATE TABLE card_dispatch_deferrals (
    project TEXT PRIMARY KEY REFERENCES projects(name) ON DELETE CASCADE,
    reason TEXT NOT NULL
);

-- Cover API moves, rule moves, archives, and direct transactional updates.
CREATE TRIGGER card_dispatch_retry_cleanup AFTER UPDATE OF column_id, archived ON cards
WHEN NEW.archived = 1 OR
     (SELECT coalesce(role, name) FROM board_columns WHERE id = NEW.column_id) <> 'ready'
BEGIN
    DELETE FROM card_dispatch_retries WHERE card = NEW.id;
END;
