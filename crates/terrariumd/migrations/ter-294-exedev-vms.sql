-- TER-294: the exe.dev VMs that Terrarium made. A row is written before
-- `new` runs and outlives its target, so the sweep finds the VM on the
-- account it was made on (the token secret `api_token` as the target's
-- project `project` opened it, with `admin_set`).
CREATE TABLE exedev_vms (
    name TEXT PRIMARY KEY,
    target TEXT NOT NULL,
    project TEXT,
    api_token TEXT NOT NULL,
    admin_set INTEGER NOT NULL DEFAULT 0,
    tag TEXT NOT NULL,
    ssh_dest TEXT,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL
);
CREATE INDEX exedev_vms_target ON exedev_vms(target);
