-- Downsampled aggregates for long-term history.
-- Raw data is kept for 7 days, then aggregated into hourly averages for 90 days,
-- then daily averages for 1 year.
CREATE TABLE IF NOT EXISTS checks_hourly (
    monitor_id  TEXT    NOT NULL,
    hour        INTEGER NOT NULL,  -- unix epoch seconds (UTC), rounded to hour
    up_count    INTEGER NOT NULL DEFAULT 0,
    down_count  INTEGER NOT NULL DEFAULT 0,
    degraded_count INTEGER NOT NULL DEFAULT 0,
    avg_latency_ms INTEGER,        -- average latency for up/degraded checks
    PRIMARY KEY (monitor_id, hour)
);

CREATE TABLE IF NOT EXISTS checks_daily (
    monitor_id  TEXT    NOT NULL,
    day         INTEGER NOT NULL,  -- unix epoch seconds (UTC), rounded to day
    up_count    INTEGER NOT NULL DEFAULT 0,
    down_count  INTEGER NOT NULL DEFAULT 0,
    degraded_count INTEGER NOT NULL DEFAULT 0,
    avg_latency_ms INTEGER,        -- average latency for up/degraded checks
    PRIMARY KEY (monitor_id, day)
);

CREATE INDEX IF NOT EXISTS idx_checks_hourly_monitor ON checks_hourly (monitor_id, hour DESC);
CREATE INDEX IF NOT EXISTS idx_checks_daily_monitor ON checks_daily (monitor_id, day DESC);
