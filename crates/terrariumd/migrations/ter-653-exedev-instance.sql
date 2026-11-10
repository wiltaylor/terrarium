-- TER-653: the exe.dev instance id of this daemon, made once. It is in the
-- name prefix of each VM that this daemon makes
-- (`terrarium-x-<id>-<target>-<tail>`), so the sweep of one daemon never
-- takes a VM of another daemon on the same exe.dev account for a leftover.
-- 8 lower case hex characters: a plain word for the exe.dev API.
CREATE TABLE exedev_instance (
    id TEXT NOT NULL
);
INSERT INTO exedev_instance (id) VALUES (lower(hex(randomblob(4))));
