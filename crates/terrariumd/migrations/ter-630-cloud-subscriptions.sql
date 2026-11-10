-- Cloud subscriptions for Claude Code (TER-630, `terrarium_core::cloud`).
-- `cloud` is the JSON of `CloudSettings` (no secret): region, project,
-- resource, base URL, model map, and monthly budget in USD.
ALTER TABLE subscriptions ADD COLUMN cloud TEXT CHECK (cloud IS NULL OR json_valid(cloud));

-- The running `costUSD` total of each Claude Code session, one row per
-- session and UTC month. `base_usd` is the total at the end of the months
-- before, so the spend of the month is `total_usd - base_usd`. A tail that
-- reads a session file again writes the same totals: nothing counts twice.
CREATE TABLE cloud_spend (
    agent        TEXT NOT NULL,
    session      TEXT NOT NULL,
    month        TEXT NOT NULL,
    subscription TEXT,
    base_usd     REAL NOT NULL DEFAULT 0,
    total_usd    REAL NOT NULL DEFAULT 0,
    updated_at   TEXT NOT NULL,
    PRIMARY KEY (agent, session, month)
);
CREATE INDEX cloud_spend_sub ON cloud_spend(subscription, month);
