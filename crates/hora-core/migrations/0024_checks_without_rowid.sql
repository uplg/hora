-- `checks` becomes a WITHOUT ROWID table keyed on (monitor_id, time).
--
-- Every check used to be stored four times over: the rowid table, the UNIQUE
-- (monitor_id, time) autoindex, the monitor-led covering index and the
-- time-led one. Nothing reads a check by its `id`; every query seeks on
-- (monitor_id, time) or on time. Clustering the table on that key makes the
-- table itself the monitor-led index: one B-tree instead of three, and an
-- insert writes two B-trees instead of four.
--
-- The monitor-led covering index it replaces was also declared `time DESC`,
-- so each new check landed at the *front* of its monitor's range and every
-- page split left two half-empty pages behind: it sat at ~50% fill. The
-- primary key is ascending (appends fill pages); SQLite walks it backwards
-- for `ORDER BY time DESC`.
--
-- Measured on a real database (139,675 checks, 13 monitors): 17.4 MB -> 8.0 MB
-- (125 -> 57 bytes per check), WAL volume per insert halved, read timings of
-- the per-monitor queries unchanged.
--
-- This rewrites the table once, inside the migration's transaction: the first
-- start after the upgrade takes a moment on a large history and needs free
-- disk space of about the database's size (see UPGRADES.md).
CREATE TABLE checks_new (
    monitor_id  TEXT    NOT NULL, -- matches a monitor id from the config
    time        INTEGER NOT NULL, -- unix epoch seconds (UTC)
    status      INTEGER NOT NULL CHECK (status IN (0, 1, 2)), -- 0 down, 1 up, 2 degraded
    latency_ms  INTEGER,          -- NULL when the probe never connected
    status_code INTEGER,          -- HTTP status code, when applicable
    error       TEXT,             -- short failure detail, when down
    source      TEXT    NOT NULL DEFAULT 'probe',
    reason      TEXT,
    PRIMARY KEY (monitor_id, time)
) WITHOUT ROWID;

-- In key order, so the new B-tree is built by appending.
INSERT INTO checks_new
    (monitor_id, time, status, latency_ms, status_code, error, source, reason)
SELECT monitor_id, time, status, latency_ms, status_code, error, source, reason
FROM checks
ORDER BY monitor_id, time;

DROP TABLE checks;
ALTER TABLE checks_new RENAME TO checks;

-- The time-led covering index (see 0017), for the batched aggregates that
-- filter on `time` alone.
CREATE INDEX idx_checks_time ON checks (time, monitor_id, status, latency_ms);

-- The roll-up tables' (monitor_id, hour|day DESC) indexes duplicated their
-- primary keys' own indexes column for column: every bucket was indexed twice
-- for nothing.
DROP INDEX IF EXISTS idx_checks_hourly_monitor;
DROP INDEX IF EXISTS idx_checks_daily_monitor;
