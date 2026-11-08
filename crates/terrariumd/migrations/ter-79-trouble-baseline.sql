-- Do not page leads for the pre-deploy history of existing cards.
CREATE TABLE card_trouble_baseline (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    seq INTEGER NOT NULL
);
INSERT INTO card_trouble_baseline VALUES (1, (SELECT coalesce(max(seq), 0) FROM events));
