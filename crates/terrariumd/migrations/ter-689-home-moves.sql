-- Seek directly to a card's newest move, without residual history filtering.
CREATE INDEX events_home_card_moves ON events(card, seq)
    WHERE kind = 'card'
      AND CASE WHEN json_valid(payload) THEN json_extract(payload, '$.change') END = 'moved';
