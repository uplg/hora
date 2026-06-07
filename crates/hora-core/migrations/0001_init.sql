-- Append-only time series of probe results. One row per check.
CREATE TABLE IF NOT EXISTS checks (
    time        INTEGER NOT NULL, -- unix epoch seconds (UTC)
    monitor_id  TEXT    NOT NULL, -- matches a monitor id from the config
    status      INTEGER NOT NULL, -- 0 = down, 1 = up, 2 = degraded
    latency_ms  INTEGER,          -- NULL when the probe never connected
    status_code INTEGER,          -- HTTP status code, when applicable
    error       TEXT              -- short failure detail, when down
);

CREATE INDEX IF NOT EXISTS idx_checks_monitor_time ON checks (monitor_id, time DESC);
