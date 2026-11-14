-- A Grok Build agent (TER-1192): `agents.harness` takes 'grok'. SQLite
-- cannot change a CHECK. A table rebuild is not safe for `agents`: with
-- foreign keys on (a migration runs in a transaction, where the pragma
-- cannot change), DROP TABLE deletes each row first, and the ON DELETE
-- CASCADE (or SET NULL) of each table that references `agents` would
-- change its rows; a RENAME of the table moves those references to the
-- old name. So the column is replaced in place, as legacy migrations
-- 9 and 24 did: add the new column with the new CHECK, copy every row,
-- drop the old column (its CHECK goes with it), rename. The column moves
-- to the end of the table; the store reads columns by name. Every other
-- column, row, index, and reference stays as it is.
ALTER TABLE agents ADD COLUMN harness_v1192 TEXT NOT NULL DEFAULT 'claude'
    CHECK (harness_v1192 IN ('claude', 'codex', 'copilot', 'pi', 'grok'));
UPDATE agents SET harness_v1192 = harness;
ALTER TABLE agents DROP COLUMN harness;
ALTER TABLE agents RENAME COLUMN harness_v1192 TO harness;
