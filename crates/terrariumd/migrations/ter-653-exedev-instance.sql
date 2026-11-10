-- TER-653: the exe.dev instance id of this daemon. It is in the name
-- prefix of each VM that this daemon makes
-- (`terrarium-x-<id>-<target>-<tail>`), so the sweep of one daemon never
-- takes a VM of another daemon on the same exe.dev account for a leftover.
-- The daemon writes the one row at its first exe.dev use, not this file:
-- a copy of a fresh database (a test template) gets its own id.
CREATE TABLE exedev_instance (
    id TEXT NOT NULL
);
