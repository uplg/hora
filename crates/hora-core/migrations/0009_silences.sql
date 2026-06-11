-- Ad-hoc alert silences (deploy windows), created via `hora silence` or
-- POST /api/silence. Checks keep being recorded; only alert transitions are
-- muted, exactly like a configured maintenance window. Expired rows are swept
-- by the pruner.
CREATE TABLE IF NOT EXISTS silences (
    id         INTEGER PRIMARY KEY,
    monitor_id TEXT    NOT NULL,  -- a monitor id, or '*' for every monitor
    until      INTEGER NOT NULL,  -- unix epoch seconds (UTC) when it expires
    reason     TEXT,
    created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_silences_monitor_until ON silences (monitor_id, until);
