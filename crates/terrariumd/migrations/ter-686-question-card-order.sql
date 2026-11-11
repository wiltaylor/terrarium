-- Card pages read linked question history, in the Inbox's display order.
CREATE INDEX questions_card_order ON questions(
    card,
    (status = 'open') DESC,
    CASE WHEN status = 'open' THEN
        CASE priority WHEN 'critical' THEN 2 WHEN 'urgent' THEN 1 ELSE 0 END
        ELSE 0 END DESC,
    CASE WHEN status = 'open' THEN created_at END ASC,
    coalesce(answered_at, updated_at) DESC,
    id
);
