-- The review rounds of a card without a review dispatcher (TER-614 R9):
-- each move of the card into a review column. A card of a project with a
-- review dispatcher shows the dispatcher's round (card_reviews) instead.
ALTER TABLE cards ADD COLUMN review_entries INTEGER NOT NULL DEFAULT 0;
-- The moves of the past, from the card events of the log.
UPDATE cards SET review_entries = (
    SELECT count(*) FROM events e
    WHERE e.card = cards.id AND e.kind = 'card'
      AND json_valid(e.payload)
      AND json_extract(e.payload, '$.change') = 'moved'
      AND (json_extract(e.payload, '$.to_role') = 'review'
           OR json_extract(e.payload, '$.to') = 'review'));
CREATE TRIGGER card_review_entry AFTER UPDATE OF column_id ON cards
WHEN NEW.column_id <> OLD.column_id
  AND EXISTS (SELECT 1 FROM board_columns k WHERE k.id = NEW.column_id
              AND (k.role = 'review' OR (k.role IS NULL AND k.name = 'review')))
BEGIN
    UPDATE cards SET review_entries = review_entries + 1 WHERE id = NEW.id;
END;
