-- Keep history reads bounded to card moves even in busy projects. Some older
-- events have non-JSON payloads, so index evaluation must guard json_extract.
CREATE INDEX events_card_moves ON events(project, seq)
    WHERE kind = 'card'
      AND CASE WHEN json_valid(payload) THEN json_extract(payload, '$.change') END = 'moved';
