-- A durable, per-attempt receipt also makes upgrade recovery idempotent.
CREATE TABLE request_fallbacks (
    thread TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    epoch INTEGER NOT NULL,
    mail TEXT NOT NULL,
    PRIMARY KEY (thread, epoch)
);
