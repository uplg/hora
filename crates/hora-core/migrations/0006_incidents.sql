-- Automatically recorded incidents from down/up transitions.
CREATE TABLE IF NOT EXISTS incidents (
    id          INTEGER PRIMARY KEY,
    monitor_id  TEXT    NOT NULL,
    started_at  INTEGER NOT NULL,  -- unix epoch seconds (UTC) when down was detected
    ended_at    INTEGER,           -- unix epoch seconds (UTC) when recovered, NULL if still down
    duration_s  INTEGER,           -- seconds between start and end, NULL if still down
    cause       TEXT,              -- upstream monitor name if topology detected it
    impacted    TEXT,              -- JSON array of impacted monitor names
    error       TEXT,              -- the error message from the down event
    created_at  INTEGER NOT NULL   -- when this record was inserted
);

CREATE INDEX IF NOT EXISTS idx_incidents_monitor_time ON incidents (monitor_id, started_at DESC);
