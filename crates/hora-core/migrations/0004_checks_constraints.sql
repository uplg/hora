-- Rebuild `checks` with a primary key, a UNIQUE(monitor_id, time) constraint (so
-- a retry/bug/manual insert can't add duplicate rows that inflate the aggregates),
-- and a CHECK on `status`. Existing rows are de-duplicated and status-filtered
-- during the copy, then the covering index is rebuilt on the new table.
CREATE TABLE checks_new (
    id          INTEGER PRIMARY KEY,
    time        INTEGER NOT NULL, -- unix epoch seconds (UTC)
    monitor_id  TEXT    NOT NULL, -- matches a monitor id from the config
    status      INTEGER NOT NULL CHECK (status IN (0, 1, 2)), -- 0 down, 1 up, 2 degraded
    latency_ms  INTEGER,          -- NULL when the probe never connected
    status_code INTEGER,          -- HTTP status code, when applicable
    error       TEXT,             -- short failure detail, when down
    UNIQUE (monitor_id, time)
);

INSERT INTO checks_new (time, monitor_id, status, latency_ms, status_code, error)
SELECT time, monitor_id, status, latency_ms, status_code, error
FROM checks
WHERE status IN (0, 1, 2)
GROUP BY monitor_id, time;

DROP TABLE checks;
ALTER TABLE checks_new RENAME TO checks;

CREATE INDEX idx_checks_monitor_time ON checks (monitor_id, time DESC, status, latency_ms);
