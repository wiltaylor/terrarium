ALTER TABLE card_deps ADD COLUMN target TEXT;
CREATE INDEX card_deps_reverse ON card_deps(depends_on, card);
CREATE TABLE card_waits (
    card TEXT PRIMARY KEY REFERENCES cards(id) ON DELETE CASCADE,
    generation TEXT NOT NULL,
    mode TEXT NOT NULL CHECK(mode IN ('all', 'any')),
    action TEXT NOT NULL,
    setter TEXT NOT NULL,
    met_at TEXT,
    acted_at TEXT,
    broken INTEGER NOT NULL DEFAULT 0
);
INSERT INTO card_waits(card, generation, mode, action, setter)
SELECT DISTINCT card, lower(hex(randomblob(16))), 'all', '{}', 'operator' FROM card_deps;
CREATE VIEW card_dependency_unmet AS
SELECT d.card, d.depends_on FROM card_deps d
JOIN cards c ON c.id = d.depends_on
JOIN board_columns k ON k.id = c.column_id
LEFT JOIN card_waits w ON w.card = d.card
WHERE w.met_at IS NULL AND c.archived = 0
  AND NOT (CASE WHEN d.target IS NULL
    THEN coalesce(k.role, k.name) IN ('accepted', 'done')
    ELSE k.name = d.target OR coalesce(k.role = d.target, 0) END)
  AND NOT (coalesce(w.mode, 'all') = 'any' AND EXISTS (
    SELECT 1 FROM card_deps a JOIN cards ac ON ac.id = a.depends_on
    JOIN board_columns ak ON ak.id = ac.column_id
    WHERE a.card = d.card AND ac.archived = 0 AND
      CASE WHEN a.target IS NULL THEN coalesce(ak.role, ak.name) IN ('accepted', 'done')
      ELSE ak.name = a.target OR coalesce(ak.role = a.target, 0) END));
