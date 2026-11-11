-- TER-294: the target kind `exedev`. A CHECK cannot change in place, so
-- the column is swapped, as in legacy migration 25.
ALTER TABLE targets ADD COLUMN kind_exedev TEXT NOT NULL DEFAULT 'host'
    CHECK (kind_exedev IN ('host', 'ssh', 'docker', 'vmlab', 'vnc', 'rdp', 'desktop', 'exedev'));
UPDATE targets SET kind_exedev = kind;
ALTER TABLE targets DROP COLUMN kind;
ALTER TABLE targets RENAME COLUMN kind_exedev TO kind;
