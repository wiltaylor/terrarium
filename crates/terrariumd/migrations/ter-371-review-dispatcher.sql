ALTER TABLE projects ADD COLUMN review_dispatcher TEXT;
CREATE TABLE card_reviews (
    card TEXT PRIMARY KEY REFERENCES cards(id) ON DELETE CASCADE,
    round INTEGER NOT NULL DEFAULT 0,
    agent TEXT,
    cap_notified INTEGER NOT NULL DEFAULT 0,
    -- 0: waiting for verdict, 1: invalidated by move, 2: verdict consumed.
    applied INTEGER NOT NULL DEFAULT 0
);
CREATE TRIGGER card_review_reset AFTER UPDATE OF column_id ON cards
WHEN NEW.column_id <> OLD.column_id
BEGIN
    UPDATE card_reviews SET applied = 1 WHERE card = NEW.id;
END;
